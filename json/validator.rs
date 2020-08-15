use crate::schema::{index, intern, Annotation, Application, Keyword, Schema, Validation, *};
use crate::{LocatedItem, LocatedProperty, Location, Number, Span, Walker};
use fxhash::FxHashSet as HashSet;
use std::borrow::Cow;
use tinyvec::TinyVec;

pub trait Context: Sized + Default + std::fmt::Debug {
    fn with_details<'sm, 'a, A>(
        loc: &'a Location<'a>,
        span: &'a Span,
        scope: &Scope<'sm, A, Self>,
        parents: &[Scope<'sm, A, Self>],
    ) -> Self
    where
        A: Annotation;

    fn span(&self) -> &Span;

    /// Build a "basic" output entry for this error occurring within this Context.
    /// See https://json-schema.org/draft/2019-09/json-schema-core.html#rfc.section.10.4.2
    fn basic_output_entry(&self, error: String) -> serde_json::Value;
}

#[derive(Debug)]
pub struct FullContext {
    pub instance_ptr: String,
    pub canonical_uri: String,
    pub keyword_location: String,
    pub span: Span,
}

impl Default for FullContext {
    fn default() -> Self {
        FullContext {
            instance_ptr: String::new(),
            canonical_uri: String::new(),
            keyword_location: String::new(),
            span: Span {
                begin: 0,
                end: 0,
                hashed: 0,
            },
        }
    }
}

impl Context for FullContext {
    fn with_details<'sm, 'a, A>(
        loc: &'a Location<'a>,
        span: &'a Span,
        scope: &Scope<'sm, A, Self>,
        parents: &[Scope<'sm, A, Self>],
    ) -> Self
    where
        A: Annotation,
    {
        FullContext {
            instance_ptr: loc.url_escaped().to_string(),
            canonical_uri: scope.schema.curi.as_str().to_owned(),
            keyword_location: scope.keyword_location(parents),
            span: Span {
                begin: span.begin,
                end: span.end,
                hashed: span.hashed,
            },
        }
    }

    fn span(&self) -> &Span {
        &self.span
    }

    fn basic_output_entry(&self, error: String) -> serde_json::Value {
        serde_json::json!({
            "keywordLocation": self.keyword_location,
            "instanceLocation": self.instance_ptr,
            "absoluteKeywordLocation": self.canonical_uri,
            "error": error,
        })
    }
}

#[derive(Debug)]
pub enum Outcome<'sm, A: Annotation> {
    Invalid(&'sm Validation),
    NotIsValid,
    AnyOfNotMatched,
    OneOfNotMatched,
    OneOfMultipleMatched,
    ReferenceNotFound(url::Url),
    Annotation(&'sm A),
}

impl<'sm, A: Annotation> Outcome<'sm, A> {
    pub fn is_error(&self) -> bool {
        match self {
            Outcome::Invalid(_)
            | Outcome::NotIsValid
            | Outcome::AnyOfNotMatched
            | Outcome::OneOfNotMatched
            | Outcome::OneOfMultipleMatched
            | Outcome::ReferenceNotFound(_) => true,
            Outcome::Annotation(_) => false,
        }
    }
}

// Default is implemented for use in TinyVec.
impl<'sm, A> Default for Outcome<'sm, A>
where
    A: Annotation,
{
    fn default() -> Self {
        Outcome::NotIsValid
    }
}

/// Build "basic" output from a set of validator outcomes.
/// See: https://json-schema.org/draft/2019-09/json-schema-core.html#rfc.section.10.4.2
pub fn build_basic_output<'sm, C: Context, A: Annotation>(
    outcomes: &[(Outcome<'sm, A>, C)],
) -> serde_json::Value {
    let errors = outcomes
        .iter()
        .filter(|(o, _)| o.is_error())
        .map(|(outcome, ctx)| ctx.basic_output_entry(format!("{:?}", outcome)))
        .collect::<Vec<_>>();

    serde_json::json!({
        "valid": errors.is_empty(),
        "errors": errors,
    })
}

type BoolVec = TinyVec<[bool; 15]>;

pub struct Scope<'sm, A, C>
where
    A: Annotation,
    C: Context,
{
    // Parent of this Scope: it's index in the validation context,
    // and the parent Application which produced this Scope.
    parent: Option<(usize, &'sm Application)>,
    // Schema evaluated by this Scope.
    schema: &'sm Schema<A>,

    // Validation result of this scope.
    invalid: bool,
    // Errors or annotations of this Scope and its children.
    outcomes: TinyVec<[(Outcome<'sm, A>, C); 4]>,
    // Outputs produced by unevaluated* of child items/properties, which
    // *may* become output of this schema iff we later determine that the
    // child wasn't evaluated by any other in-place application.
    //
    // Each entry tracks its applicable child index, which is compared with
    // |evaluated|'s final result (after merging all valid in-place applications)
    // to identify outputs not already covered by an evaluated child.
    //
    // Conditioned on C::RETAIN_OUTPUT.
    outcomes_unevaluated: TinyVec<[(Outcome<'sm, A>, C, usize); 4]>,

    // Interned properties which were observed while evaluating this scope.
    seen_interned: intern::Set,

    // Validation result of a "if" in-place application.
    // Popped "else" and "then" applications examine this to determine if errors project to the parent.
    valid_if: Option<bool>,
    // Validation results of "anyOf" in-place applications, indexed by bit.
    // If the schema has no "anyOf" applications, this is [1].
    valid_any_of: BoolVec,
    // Validation results of "oneOf" in-place applications, indexed by bit.
    // If the schema has no "oneOf" applications, this is [1].
    valid_one_of: BoolVec,
    // Number of items which validated against a "contains" item application.
    valid_contains: usize,
    // unique_items is the set of encountered item hashes.
    // It's None unless this Scope's Schema has a "uniqueItems" validation.
    unique_items: Option<HashSet<u64>>,
    // Evaluated captures whether each child index was evaluated by an applied "properties",
    // "patternProperties", "additionalProperties", "items", or "additionalItems" application
    // of this scope, or a valid in-place application scope thereof.
    evaluated: BoolVec,
    // Validation results of speculative "unevaluatedProperties" / "unevaluatedItems"
    // applications. If no children had an "unevaluated" application, this is empty.
    valid_unevaluated: BoolVec,
}

impl<'sm, A, C> Scope<'sm, A, C>
where
    A: Annotation,
    C: Context,
{
    fn new(parent: Option<(usize, &'sm Application)>, schema: &'sm Schema<A>) -> Scope<'sm, A, C> {
        Scope {
            parent,
            schema,
            invalid: false,
            outcomes: TinyVec::default(),
            outcomes_unevaluated: TinyVec::default(),
            seen_interned: 0 as intern::Set,
            valid_if: None,
            valid_any_of: BoolVec::default(),
            valid_one_of: BoolVec::default(),
            valid_contains: 0,
            unique_items: None,
            evaluated: BoolVec::default(),
            valid_unevaluated: BoolVec::default(),
        }
    }

    fn add_outcome(&mut self, o: Outcome<'sm, A>, c: C) {
        // println!("\t\t\t\t  {:?} @ {:?}", o, c);
        self.outcomes.push((o, c));
    }

    fn keyword_location(&self, parents: &[Scope<'sm, A, C>]) -> String {
        match self.parent {
            Some((ind, app)) => {
                let s = parents[ind].keyword_location(parents);
                app.extend_fragment_pointer(s)
            }
            None => "#".to_owned(),
        }
    }

    fn dynamic_base(&self, parents: &[Scope<'sm, A, C>]) -> Option<&'sm url::Url> {
        let mut r = None;
        if let Some((ind, _)) = self.parent {
            r = parents[ind].dynamic_base(parents);
        }
        r.or_else(|| {
            self.schema.kw.first().and_then(|kw| match kw {
                Keyword::RecursiveAnchor => Some(&self.schema.curi),
                _ => None,
            })
        })
    }
}

pub struct Validator<'sm, A, C>
where
    A: Annotation,
    C: Context,
{
    index: &'sm index::Index<'sm, A>,
    scopes: Vec<Scope<'sm, A, C>>,
    active_offsets: Vec<usize>,
}

impl<'v, A, C> Walker for Validator<'v, A, C>
where
    A: Annotation,
    C: Context,
{
    fn push_property<'a>(&mut self, span: &Span, loc: &'a LocatedProperty<'a>) {
        //println!(
        //    "\t\t\t\tpush_property {} @ {:?}",
        //    Location::Property(*loc),
        //    span
        //);

        use Application::{
            AdditionalProperties, PatternProperties, Properties, PropertyNames,
            UnevaluatedProperties,
        };
        use Keyword::Application as KWApp;

        let active_from = *self.active_offsets.last().unwrap();
        let active_to = self.scopes.len();

        // Push propertyNames applications to evaluate the property name.
        // (We cannot use an iterator because self.scopes is mutated).
        for scope_index in active_from..active_to {
            for kw in &self.scopes[scope_index].schema.kw {
                if let KWApp(app @ PropertyNames, sub) = &kw {
                    self.scopes.push(Scope::new(Some((scope_index, app)), sub));
                }
            }
        }
        self.expand_scopes(active_to, span, &Location::Property(*loc));
        self.active_offsets.push(active_to);

        // Apply the property name as a string, which pops propertyName applications.
        self.pop_str(span, &loc.parent, loc.name);

        // Now identify application keywords of current scopes which apply to
        // the current property, and push each to validate its forthcoming value.
        for scope_index in active_from..active_to {
            let scope = &mut self.scopes[scope_index];
            let mut evaluated = false;

            let interned = scope.schema.tbl.lookup(loc.name);
            scope.seen_interned |= interned;

            for kw in &scope.schema.kw {
                let (app, sub) = match kw {
                    KWApp(app, sub) => (app, sub),
                    _ => continue,
                };
                // Property applications have preference rules (which keywords are sorted by).
                // C.f. https://json-schema.org/draft/2019-09/json-schema-core.html#rfc.section.9.3.2
                let evaluates = match app {
                    // Properties always applies on equality of (interned) property name.
                    Properties {
                        name_interned: i, ..
                    } if *i == interned => true,
                    // PatternProperties always applies on regex match of property name.
                    PatternProperties { re } if re.is_match(loc.name) => true,
                    // AdditionalProperties applies if Properties and PatternProperties haven't.
                    AdditionalProperties if !evaluated => true,
                    // Finally, UnevaluatedProperties applies if no other application evaluates.
                    UnevaluatedProperties if !evaluated => false,

                    _ => continue,
                };
                self.scopes.push(Scope::new(Some((scope_index, app)), sub));
                evaluated = evaluates;
            }
            self.scopes[scope_index].evaluated.push(evaluated);
        }
        self.expand_scopes(active_to, span, &Location::Property(*loc));
        self.active_offsets.push(active_to);
    }

    fn push_item<'a>(&mut self, span: &Span, loc: &'a LocatedItem<'a>) {
        //println!("\t\t\t\tpush_item {} @ {:?}", Location::Item(*loc), span);

        use Application::{AdditionalItems, Contains, Items, UnevaluatedItems};
        use Keyword::Application as KWApp;

        let active_from = *self.active_offsets.last().unwrap();
        let active_to = self.scopes.len();

        for scope_index in active_from..active_to {
            let scope = &mut self.scopes[scope_index];
            let mut evaluated = false;
            let mut indexed_items = false;

            for kw in &scope.schema.kw {
                let (app, sub) = match kw {
                    KWApp(app, sub) => (app, sub),
                    _ => continue,
                };
                // Item applications also have preference rules (which keywords are sorted by).
                // C.f https://json-schema.org/draft/2019-09/json-schema-core.html#rfc.section.9.3.1
                let evaluates = match app {
                    // Items without an index always applies.
                    Items { index: None } => true,
                    // Items with an index matches on location index equality.
                    Items { index: Some(i) } => {
                        indexed_items = true;
                        if *i != loc.index {
                            continue;
                        }
                        true
                    }
                    // AdditionalItems evaluates if indexed Items exist and none matched.
                    AdditionalItems if indexed_items && !evaluated => true,
                    // Contains applies but does not evaluate.
                    Contains => false,
                    // Finally, UnevaluatedItems applies if no other application evaluates.
                    UnevaluatedItems if !evaluated => false,

                    _ => continue,
                };
                self.scopes.push(Scope::new(Some((scope_index, app)), sub));
                evaluated |= evaluates;
            }
            self.scopes[scope_index].evaluated.push(evaluated);
        }
        self.expand_scopes(active_to, span, &Location::Item(*loc));
        self.active_offsets.push(active_to);
    }

    fn pop_object<'a>(&mut self, span: &Span, loc: &'a Location<'a>, num_properties: usize) {
        //println!(
        //    "\t\t\t\tpop_object {:?} @ {}:{:?}",
        //    num_properties, loc, span
        //);

        self.check_validations(span, loc, |validation, scope| -> bool {
            use Validation::*;

            match validation {
                False => false,
                Type(expect) => *expect & types::OBJECT != types::INVALID,
                Const(literal) => literal.hash == span.hashed,
                Enum { variants } => variants.iter().any(|l| l.hash == span.hashed),
                Required {
                    props_interned: set,
                    ..
                } => *set & scope.seen_interned == *set,
                MinProperties(bound) => num_properties >= *bound,
                MaxProperties(bound) => num_properties <= *bound,
                DependentRequired {
                    if_interned: if_,
                    then_interned: then_,
                    ..
                } => (scope.seen_interned & *if_ == 0) || (scope.seen_interned & *then_ == *then_),
                _ => true,
            }
        });
        self.pop(span, loc);
    }

    fn pop_array<'a>(&mut self, span: &Span, loc: &'a Location<'a>, num_items: usize) {
        //println!("\t\t\t\tpop_array {:?} @ {}:{:?}", num_items, loc, span);

        self.check_validations(span, loc, |validation, scope| -> bool {
            use Validation::*;

            match validation {
                False => false,
                Type(expect) => *expect & types::ARRAY != types::INVALID,
                Const(literal) => literal.hash == span.hashed,
                Enum { variants } => variants.iter().any(|l| l.hash == span.hashed),
                MinItems(bound) => *bound <= num_items,
                MaxItems(bound) => *bound >= num_items,
                MinContains(bound) => *bound <= scope.valid_contains,
                MaxContains(bound) => *bound >= scope.valid_contains,
                _ => true,
            }
        });
        self.pop(span, loc);
    }

    fn pop_bool<'a>(&mut self, span: &Span, loc: &'a Location<'a>, _b: bool) {
        //println!("\t\t\t\tpop_bool {:?} @ {}:{:?}", b, loc, span);

        self.check_validations(span, loc, |validation, _| -> bool {
            use Validation::*;

            match validation {
                False => false,
                Type(expect) => *expect & types::BOOLEAN != types::INVALID,
                Const(literal) => literal.hash == span.hashed,
                Enum { variants } => variants.iter().any(|l| l.hash == span.hashed),
                _ => true,
            }
        });
        self.pop(span, loc);
    }

    fn pop_numeric<'a>(&mut self, span: &Span, loc: &'a Location<'a>, num: Number) {
        //println!("\t\t\t\tpop_numeric {:?} @ {}:{:?}", num, loc, span);

        self.check_validations(span, loc, |validation, _| -> bool {
            use Validation::*;

            match validation {
                False => false,
                Type(expect) => {
                    let actual = match num {
                        // The json schema spec says that the "integer" type must match
                        // "any number with a zero fractional part":
                        // https://json-schema.org/draft/2019-09/json-schema-validation.html#rfc.section.6.1.1
                        // So if there's an actual fractional part, then only "number" is valid,
                        // but for any other numeric value, then "integer" is also valid.
                        Number::Float(value) if value.fract() > 0.0 => types::NUMBER,
                        _ => types::INTEGER | types::NUMBER,
                    };
                    *expect & actual != types::INVALID
                }
                Const(literal) => literal.hash == span.hashed,
                Enum { variants } => variants.iter().any(|l| l.hash == span.hashed),
                Minimum(bound) => num >= *bound,
                Maximum(bound) => num <= *bound,
                ExclusiveMinimum(bound) => num > *bound,
                ExclusiveMaximum(bound) => num < *bound,
                MultipleOf(bound) => num.is_multiple_of(bound),
                _ => true,
            }
        });
        self.pop(span, loc);
    }

    fn pop_str<'a>(&mut self, span: &Span, loc: &'a Location<'a>, s: &'a str) {
        //println!(
        //    "\t\t\t\tpop_str {:?} len {:?} @ {}:{:?}",
        //    s,
        //    s.chars().count(),
        //    loc,
        //    span
        //);

        self.check_validations(span, loc, |validation, _| -> bool {
            use Validation::*;

            match validation {
                False => false,
                Type(expect) => *expect & types::STRING != types::INVALID,
                Const(literal) => literal.hash == span.hashed,
                Enum { variants } => variants.iter().any(|l| l.hash == span.hashed),
                MinLength(bound) => *bound <= s.chars().count(),
                MaxLength(bound) => *bound >= s.chars().count(),
                Pattern(re) => re.is_match(s),
                _ => true,
            }
        });
        self.pop(span, loc);
    }

    fn pop_null<'a>(&mut self, span: &Span, loc: &'a Location<'a>) {
        //println!("\t\t\t\tpop_null <null> @ {}:{:?}", loc, span);

        self.check_validations(span, loc, |validation, _| -> bool {
            use Validation::*;

            match validation {
                False => false,
                Type(expect) => *expect & types::NULL != types::INVALID,
                Const(literal) => literal.hash == span.hashed,
                Enum { variants } => variants.iter().any(|l| l.hash == span.hashed),
                _ => true,
            }
        });
        self.pop(span, loc);
    }
}

impl<'sm, A, C> Validator<'sm, A, C>
where
    A: Annotation,
    C: Context,
{
    /// Return a new Validator, which must be reset prior to use.
    pub fn new(index: &'sm index::Index<'sm, A>) -> Validator<'sm, A, C> {
        Validator {
            index,
            scopes: Vec::new(),
            active_offsets: Vec::new(),
        }
    }

    /// Prepare the Validator to begin validation of the indexed schema |uri|.
    /// May be called more than once on a Validator, to re-use it for multiple validations.
    pub fn prepare(&mut self, uri: &url::Url) -> Result<(), index::Error> {
        let schema = self.index.must_fetch(uri)?;

        self.scopes.truncate(0);
        self.scopes.push(Scope::new(None, schema));

        self.active_offsets.truncate(0);
        self.active_offsets.push(0);

        let span = Span {
            begin: 0,
            end: 0,
            hashed: 0,
        };
        self.expand_scopes(0, &span, &Location::Root);

        Ok(())
    }

    /// Invalid is true if the input didn't validate against the schema.
    pub fn invalid(&self) -> bool {
        self.scopes[0].invalid
    }
    /// Outcomes returns validation errors, if any, as well as collected annotations.
    pub fn outcomes(&self) -> &[(Outcome<'sm, A>, C)] {
        &self.scopes[0].outcomes
    }

    fn check_validations<'a, F>(&mut self, span: &Span, loc: &'a Location<'a>, func: F)
    where
        F: Fn(&Validation, &Scope<'sm, A, C>) -> bool,
    {
        let from = *self.active_offsets.last().unwrap();
        let to = self.scopes.len();

        for ind in from..to {
            let (parents, tail) = self.scopes.split_at_mut(ind);
            let scope = &mut tail[0];

            for kw in &scope.schema.kw {
                let val = match kw {
                    Keyword::Validation(val) => val,
                    _ => continue,
                };

                if !func(val, scope) {
                    scope.invalid = true;
                    scope.add_outcome(
                        Outcome::Invalid(val),
                        C::with_details(loc, span, scope, parents),
                    );
                }
            }
        }
    }

    fn pop<'l>(&mut self, span: &Span, loc: &'l Location<'l>) {
        let pivot = *self.active_offsets.last().unwrap();
        let limit = self.scopes.len();

        for i in (pivot..limit).rev() {
            let (scope, parents) = self.scopes.split_last_mut().unwrap();
            Validator::finish_scope(scope, parents, span, loc);

            // Unwind and pop all but the root-most Scope.
            if i != 0 {
                Validator::unwind_scope(scope, parents, loc);
                self.scopes.pop();
            }
        }
        self.active_offsets.pop();

        // Walk re-activated scopes. If any have non-None unique_items, AND
        // our |loc| is a Location::Item, then collect the unique item hash.
        let limit = pivot;
        let pivot = *self.active_offsets.last().unwrap_or(&0);

        for i in (pivot..limit).rev() {
            let (parents, scope) = self.scopes.split_at_mut(i);
            let scope = &mut scope[0];

            if let Some(set) = &mut scope.unique_items {
                if let Location::Item(_) = loc {
                    if set.insert(span.hashed) {
                        continue; // Hash not previously seen.
                    }

                    // Duplicate item. If this scope has a UniqueItems validation, invalidate it.
                    for kw in &scope.schema.kw {
                        if let Keyword::Validation(val @ Validation::UniqueItems) = kw {
                            scope.invalid = true;
                            scope.add_outcome(
                                Outcome::Invalid(val),
                                C::with_details(loc, span, scope, parents),
                            );
                        }
                    }
                }
            }
        }
    }

    fn finish_scope<'l>(
        scope: &mut Scope<'sm, A, C>,
        parents: &[Scope<'sm, A, C>],
        span: &Span,
        loc: &'l Location<'l>,
    ) {
        use Outcome::*;

        // "anyOf": assert at least one application was valid.
        if !scope.valid_any_of.is_empty() && !scope.valid_any_of.contains(&true) {
            scope.invalid = true;
            scope.add_outcome(AnyOfNotMatched, C::with_details(loc, span, scope, parents));
        }

        // "oneOf": assert exactly one application was valid (or there were none).
        let c = scope.valid_one_of.is_empty() as usize
            + scope.valid_one_of.iter().filter(|&b| *b).count();
        if c == 0 {
            scope.invalid = true;
            scope.add_outcome(OneOfNotMatched, C::with_details(loc, span, scope, parents));
        } else if c > 1 {
            scope.invalid = true;
            scope.add_outcome(
                OneOfMultipleMatched,
                C::with_details(loc, span, scope, parents),
            );
        }

        // Note that "allOf", "if", "then", "else", and "not" keyword
        // applications of this scope already applied their outcomes, when their
        // respective scopes were unwound.

        // For each of |speculative_outcomes|, add it to |outcomes| if its child
        // index was not matched by this scope or an in-place application thereof.
        let evaluated = scope.evaluated.as_slice();
        for (outcome, ctx, ind) in scope.outcomes_unevaluated.drain(..) {
            if !evaluated[ind] {
                scope.outcomes.push((outcome, ctx));
            }
        }

        // Now fold successful |speculative| applications into |evaluated|.
        for (eval, uneval) in scope
            .evaluated
            .iter_mut()
            .zip(scope.valid_unevaluated.iter())
        {
            *eval |= *uneval;
        }

        // If we speculatively examined *any* children, and there exists a
        // child that was not evaluated, then fail this scope.
        if !scope.valid_unevaluated.is_empty() && scope.evaluated.contains(&false) {
            scope.invalid = true;
        }

        // At this point, scope.invalid is the final validation status of this scope.

        // "not": handle by inverting the scope's validation status.
        if let Some((_, &Application::Not)) = scope.parent {
            scope.outcomes.clear();

            scope.invalid = !scope.invalid;
            if scope.invalid {
                scope.add_outcome(NotIsValid, C::with_details(loc, span, scope, parents));
            }
        }

        // Attach schema annotation keywords to the scope's outcome.
        if !scope.invalid {
            for kw in &scope.schema.kw {
                if let Keyword::Annotation(a) = kw {
                    scope.add_outcome(Annotation(a), C::with_details(loc, span, scope, parents));
                }
            }
        }
    }

    fn unwind_scope<'b>(
        scope: &mut Scope<'sm, A, C>,
        parents: &mut [Scope<'sm, A, C>],
        loc: &'b Location<'b>,
    ) {
        //println!("unwind_scope {} {:?} '{}' '{}'", scope.invalid, scope.evaluated, scope.keyword_location(parents), scope.schema.curi);

        let (parent, app) = scope.parent.unwrap();
        let parent = &mut parents[parent];

        enum Handle {
            RequiredInPlace,
            RequiredChild,
            OptionalInPlace,
            UnevaluatedChild,
            Ignore,
        }
        use Handle::*;

        use Application as App;
        match match app {
            App::Def { .. } => panic!("unexpected Def"),

            // In-place keywords which must always validate.
            App::AllOf { .. } | App::Ref(_) | App::RecursiveRef(_) | App::Not => RequiredInPlace,

            // In-place keywords which must validate subject to the state
            // of a previously-collected annotation.
            App::Then => match parent.valid_if {
                Some(true) => RequiredInPlace,
                _ => Ignore,
            },
            App::Else => match parent.valid_if {
                Some(false) => RequiredInPlace,
                _ => Ignore,
            },
            App::DependentSchema { if_interned: i, .. } => {
                if (*i & parent.seen_interned) != 0 {
                    RequiredInPlace
                } else {
                    Ignore
                }
            }

            // Applications which collect annotations but don't directly invalidate their parent.
            App::If => {
                parent.valid_if = Some(!scope.invalid);
                OptionalInPlace
            }
            App::AnyOf { .. } => {
                parent.valid_any_of.push(!scope.invalid);
                OptionalInPlace
            }
            App::OneOf { .. } => {
                parent.valid_one_of.push(!scope.invalid);
                OptionalInPlace
            }
            App::Contains { .. } => {
                if !scope.invalid {
                    parent.valid_contains += 1;
                }
                OptionalInPlace
            }

            // Child applications which must always succeed.
            App::PatternProperties { .. }
            | App::AdditionalProperties
            | App::Items { .. }
            | App::Properties { .. }
            | App::PropertyNames
            | App::AdditionalItems => RequiredChild,

            // Speculative "unevaluated" child applications.
            App::UnevaluatedProperties => UnevaluatedChild,
            App::UnevaluatedItems => UnevaluatedChild,
        } {
            // Required in-place project validity & outcomes, and also update evaluated
            // annotations of their parent.
            RequiredInPlace => {
                parent.invalid |= scope.invalid;
                parent.outcomes.extend(scope.outcomes.drain(..));

                if !scope.invalid {
                    for (parent_eval, scope_eval) in
                        parent.evaluated.iter_mut().zip(scope.evaluated.iter())
                    {
                        *parent_eval |= *scope_eval;
                    }
                }
            }
            // Conditional scopes update parent.outcomes only if valid,
            // or if debugging is on. They never invalidate the parent.
            OptionalInPlace => {
                if !scope.invalid {
                    parent.outcomes.extend(scope.outcomes.drain(..));

                    for (parent_eval, scope_eval) in
                        parent.evaluated.iter_mut().zip(scope.evaluated.iter())
                    {
                        *parent_eval |= *scope_eval;
                    }
                } else {
                    //parent.outcomes_debug.extend(scope.outcomes.drain(..));
                }
            }
            // Required children project validity and outcomes to their parent.
            RequiredChild => {
                parent.invalid |= scope.invalid;
                parent.outcomes.extend(scope.outcomes.drain(..));
            }
            // Unevaluated scopes update parent.valid_unevaluated,
            // and scope.outcomes extend parent.outcomes_unevaluated.
            // They don't directly invalidate the parent.
            UnevaluatedChild => {
                let child_index = match loc {
                    Location::Item(LocatedItem { index, .. }) => *index,
                    Location::Property(LocatedProperty { index, .. }) => *index,
                    Location::Root => panic!("unexpected root"),
                };

                for (o, c) in scope.outcomes.drain(..) {
                    parent.outcomes_unevaluated.push((o, c, child_index));
                }
                // Applications of unevaluated* will skip children matched by "items",
                // "properties", or "patternProperties". Fill in any holes before appending
                // this validation result.
                for _ in parent.valid_unevaluated.len()..child_index {
                    parent.valid_unevaluated.push(false);
                }
                parent.valid_unevaluated.push(!scope.invalid);
            }
            Ignore => (),
        }
    }

    fn expand_scope<'a>(&mut self, index: usize, span: &Span, loc: &'a Location<'a>) {
        use Application::{
            AllOf, AnyOf, DependentSchema, Else, If, Not, OneOf, RecursiveRef, Ref, Then,
        };

        //println!("expand_scope '{}' '{}'", self.scopes[index].keyword_location(&self.scopes), self.scopes[index].schema.curi);

        for kw in &self.scopes[index].schema.kw {
            // Skip all non-application keywords.
            let (app, schema) = match &kw {
                Keyword::Application(app, schema) => (app, schema),
                Keyword::Validation(Validation::UniqueItems) => {
                    self.scopes[index].unique_items = Some(HashSet::default());
                    continue;
                }
                _ => continue,
            };

            // Determine the schema of the keyword. Usually this is the local |schema|,
            // but it could be an indexed referent.
            let (mut schema, redirect) = match app {
                Ref(uri) => (schema, Some(Cow::Borrowed(uri))),
                RecursiveRef(uri) => {
                    let scope = &self.scopes[index];
                    // Recursive base is that of the top-most scope having RecursiveAnchor,
                    // or our own canonical URI if no such scope exists.
                    let base = scope.dynamic_base(&self.scopes);
                    let base = base.unwrap_or(&scope.schema.curi);

                    // Join base with |uri| to derive the canonical reference URI.
                    // Note |uri| was confirmed to parse correctly when joined with an
                    // arbitrary base at schema build-time.
                    let mut uri: Cow<url::Url> = Cow::Owned(base.join(uri).unwrap());
                    // Canonical-ize for index lookup by stripping an empty fragment component.
                    if let Some("") = uri.fragment() {
                        uri.to_mut().set_fragment(None);
                    }
                    (schema, Some(uri))
                }
                AnyOf { .. }
                | AllOf { .. }
                | OneOf { .. }
                | Not
                | If
                | Then
                | Else
                | DependentSchema { .. } => (schema, None),

                _ => continue, // Not an in-place application.
            };

            if let Some(uri) = redirect {
                schema = match self.index.fetch(&uri) {
                    None => {
                        let ctx = C::with_details(loc, span, &self.scopes[index], &self.scopes);
                        self.scopes[index].invalid = true;
                        self.scopes[index]
                            .add_outcome(Outcome::ReferenceNotFound(uri.into_owned()), ctx);
                        continue;
                    }
                    Some(schema) => schema,
                }
            }
            self.scopes.push(Scope::new(Some((index, app)), schema));
        }
    }

    fn expand_scopes<'a>(&mut self, mut pivot: usize, span: &Span, loc: &'a Location<'a>) {
        // For each Scope in range pivot.., push all in-place applications of its schema.
        while pivot != self.scopes.len() {
            self.expand_scope(pivot, span, loc);
            pivot += 1;
        }
    }
}
