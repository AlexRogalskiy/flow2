package main

import (
	"context"
	"fmt"
	"io/ioutil"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"time"

	"github.com/estuary/flow/go/flow"
	pf "github.com/estuary/flow/go/protocols/flow"
	"github.com/estuary/flow/go/testing"
	"github.com/fatih/color"
	log "github.com/sirupsen/logrus"
	"go.etcd.io/etcd/clientv3"
	"go.gazette.dev/core/broker/client"
	"go.gazette.dev/core/broker/fragment"
	pb "go.gazette.dev/core/broker/protocol"
	mbp "go.gazette.dev/core/mainboilerplate"
	"google.golang.org/grpc"
)

type cmdTest struct {
	Source    string `long:"source" required:"true" description:"Catalog source file or URL to build"`
	Directory string `long:"directory" default:"." description:"Build directory"`
}

func (cmd cmdTest) Execute(_ []string) (retErr error) {
	var failed []string
	// This is temporary (...pause for groans) until we modify the gazette mainboilerplate package
	// to stop printing stacktraces and panicing on errors. The goal is to be able to simply return
	// a "tests failed" error in the future.
	defer func() {
		if retErr == nil { // Failing tests are expected, so we don't return an error in that case.
			// Exit code will be the number of failed tests
			var nFailed = len(failed)
			// Just in case someone has a huge number of failed tests, this will prevent us from
			// accidentally setting the exit code to 0 (all but the low 8 bits are ignored, and values
			// in the range 128-256 are given special meaning).
			if nFailed > 127 {
				nFailed = 127
			}
			os.Exit(nFailed)
		}
	}()

	defer mbp.InitDiagnosticsAndRecover(Config.Diagnostics)()
	initLog(Config.Log)

	log.WithFields(log.Fields{
		"config":    Config,
		"version":   mbp.Version,
		"buildDate": mbp.BuildDate,
	}).Info("flowctl configuration")

	var err error
	if cmd.Directory, err = filepath.Abs(cmd.Directory); err != nil {
		return fmt.Errorf("filepath.Abs: %w", err)
	}

	// Create a temp directory, used for:
	// * Storing our built catalog database.
	// * Etcd storage and UDS sockets.
	// * NPM worker UDS socket.
	// * "Persisted" fragment files.

	tempdir, err := ioutil.TempDir("", "flow-test")
	if err != nil {
		return fmt.Errorf("creating temp directory: %w", err)
	}
	defer os.RemoveAll(tempdir)

	built, err := buildCatalog(pf.BuildAPI_Config{
		CatalogPath:       filepath.Join(tempdir, "catalog.db"),
		Directory:         cmd.Directory,
		Source:            cmd.Source,
		TypescriptCompile: true,
		TypescriptPackage: false,

		// Install a testing override rule that applies after other rules,
		// disables multi-broker replication, and uses a file:// fragment store.
		ExtraJournalRules: &pf.JournalRules{
			Rules: []pf.JournalRules_Rule{
				{
					// Order after other rules.
					Rule: "\uFFFF\uFFFF-testing-overrides",
					Template: pb.JournalSpec{
						Replication: 1,
						Fragment: pb.JournalSpec_Fragment{
							Stores:           []pb.FragmentStore{"file:///"},
							CompressionCodec: pb.CompressionCodec_SNAPPY,
						},
					},
				},
			},
		},
	})
	if err != nil {
		return err
	}

	// Spawn Etcd and NPM worker processes for cluster use.
	etcd, etcdClient, err := startEtcd(tempdir)
	if err != nil {
		return err
	}
	defer stopWorker(etcd)

	var lambdaJSUDS = filepath.Join(tempdir, "lambda-js")
	jsWorker, err := startJSWorker(cmd.Directory, lambdaJSUDS)
	if err != nil {
		return err
	}
	defer stopWorker(jsWorker)

	// Configure and start the cluster.
	var cfg = testing.ClusterConfig{
		Context:            context.Background(),
		DisableClockTicks:  true,
		Etcd:               etcdClient,
		EtcdCatalogPrefix:  "/flowctl-test/catalog",
		EtcdBrokerPrefix:   "/flowctl-test/broker",
		EtcdConsumerPrefix: "/flowctl-test/runtime",
	}
	cfg.ZoneConfig.Zone = "local"
	pb.RegisterGRPCDispatcher(Config.Zone)

	// Apply catalog task specifications to the cluster.
	if _, err := flow.ApplyCatalogToEtcd(cfg.Context, cfg.Etcd,
		cfg.EtcdCatalogPrefix, built, lambdaJSUDS, ""); err != nil {
		return err
	}

	fragment.FileSystemStoreRoot = tempdir
	defer client.InstallFileTransport(fragment.FileSystemStoreRoot)()

	cluster, err := testing.NewCluster(cfg)
	if err != nil {
		return fmt.Errorf("NewCluster: %w", err)
	}

	// Apply derivation shard specs.
	if err = applyDerivationShardsTODO(built, cluster.Shards); err != nil {
		return fmt.Errorf("applying derivation shards: %w", err)
	}

	// Run all test cases.
	var graph = testing.NewGraph(built.Derivations)
	fmt.Println("Running ", len(built.Tests), " tests...")
	for _, testCase := range built.Tests {
		fmt.Print(testCase.Test, ": ")

		if scope, err := testing.RunTestCase(graph, cluster, &testCase); err != nil {
			fmt.Printf("%s\n", red("FAILED"))
			fmt.Println(red("ERROR"), "at", yellow(scope), ":")
			fmt.Println(err)
			failed = append(failed, testCase.Test)
		} else {
			fmt.Print(green("PASSED"), "\n")
		}
		cluster.Consumer.ClearRegistersForTest(cfg.Context)
	}

	// Summarize the failed tests at the end so that it's easier to see in case there's a lot of
	// error output above.
	if len(failed) > 0 {
		fmt.Printf("\n%s\n", red("Failed:"))
		for _, t := range failed {
			fmt.Println(t)
		}
	}
	fmt.Printf("\nRan %d tests, %d passed, %d failed\n",
		len(built.Tests), len(built.Tests)-len(failed), len(failed))

	if err := cluster.Stop(); err != nil {
		return fmt.Errorf("stopping cluster: %w", err)
	}
	return nil
}

func startEtcd(tmpdir string) (*exec.Cmd, *clientv3.Client, error) {
	var cmd = exec.Command("etcd",
		"--listen-peer-urls", "unix://peer.sock:0",
		"--listen-client-urls", "unix://client.sock:0",
		"--advertise-client-urls", "unix://client.sock:0",
	)
	// The Etcd --log-level flag was added in v3.4. Use it's environment variable
	// version to remain compatible with older `etcd` binaries.
	cmd.Env = append(cmd.Env, "ETCD_LOG_LEVEL=error", "ETCD_LOGGER=zap")
	cmd.Env = append(cmd.Env, os.Environ()...)

	cmd.Dir = tmpdir
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	// Deliver a SIGTERM to the process if this thread should die uncleanly.
	cmd.SysProcAttr = &syscall.SysProcAttr{Pdeathsig: syscall.SIGTERM}
	// Place child its own process group, so that terminal SIGINT isn't delivered
	// from the terminal and so that we may close leases properly.
	cmd.SysProcAttr.Setpgid = true

	log.WithFields(log.Fields{"args": cmd.Args, "dir": cmd.Dir}).Info("starting etcd")
	if err := cmd.Start(); err != nil {
		return nil, nil, fmt.Errorf("starting etcd: %w", err)
	}

	etcdClient, err := clientv3.New(clientv3.Config{
		Endpoints:   []string{"unix://" + cmd.Dir + "/client.sock:0"},
		DialTimeout: 5 * time.Second,
		DialOptions: []grpc.DialOption{grpc.WithBlock()},
		// Require a reasonably recent server cluster.
		RejectOldCluster: true,
	})
	if err != nil {
		return nil, nil, fmt.Errorf("building etcd client: %w", err)
	}

	// Verify the client works.
	if _, err = etcdClient.Get(context.Background(), "test-key"); err != nil {
		return nil, nil, fmt.Errorf("verifying etcd client: %w", err)
	}

	// Arrange to close the |etcdClient| as soon as the process completes.
	// We do this because ctrl-C sent to `flowctl develop` will also immediately
	// propagate to the `etcd` binary; as part of normal shutdown we'll try to
	// release associated Etcd leases, and will wedge for ~10 seconds trying to
	// do so before timing out and bailing out.
	go func() {
		_, _ = cmd.Process.Wait()
		etcdClient.Close()
	}()

	return cmd, etcdClient, nil
}

func startJSWorker(dir, socketPath string) (*exec.Cmd, error) {
	var cmd = exec.Command("node", "dist/flow_generated/flow/main.js")
	_ = os.Remove(socketPath)

	cmd.Env = append(cmd.Env, os.Environ()...)
	cmd.Env = append(cmd.Env, fmt.Sprintf("SOCKET_PATH=%s", socketPath))

	cmd.Dir = dir
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	log.WithField("args", cmd.Args).Info("starting node")

	if err := flow.StartCmdAndReadReady(cmd); err != nil {
		return nil, fmt.Errorf("failed to start JS worker: %w", err)
	}
	return cmd, nil
}

func stopWorker(cmd *exec.Cmd) {
	_ = cmd.Process.Signal(syscall.SIGTERM)
	_ = cmd.Wait()
}

var green = color.New(color.FgGreen).SprintFunc()
var yellow = color.New(color.FgYellow).SprintFunc()
var red = color.New(color.FgRed).SprintFunc()
