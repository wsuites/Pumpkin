use crate::context::command_context::{
    CommandContext, CommandContextBuilder, ParsedArgument, ParsedNode, take_skip_result,
};
use crate::context::string_range::StringRange;
use crate::errors::command_syntax_error::CommandSyntaxError;
use crate::node::Command;
use crate::node::RedirectModifier;
use crate::node::attached::NodeId;
use crate::node::dispatcher::{CommandDispatcher, ResultConsumer};
use crate::node::tree::Tree;
use crate::source::{CommandSource, ReturnValue};
use rustc_hash::FxHashMap;
use std::sync::{Arc, RwLock};

/// One node of a compiled command chain.
///
/// Everything a [`CommandContext`] needs, kept apart from the source so a
/// forked source (`execute as`, `run`) rebuilds the context without copying
/// the parse. Function bodies and `execute run` chains run the same line
/// thousands of times a tick, and this is the part that used to be cloned.
struct CachedStage<S: CommandSource> {
    arguments: Arc<FxHashMap<String, Arc<ParsedArgument>>>,
    root: NodeId,
    nodes: Arc<[ParsedNode]>,
    range: StringRange,
    modifier: RedirectModifier<S>,
    forks: bool,
    command: Option<Command<S>>,
}

/// A fully parsed command that no longer borrows its dispatcher or source.
///
/// Vanilla compiles function files once and then only runs them. Parsing a long
/// datapack line costs far more than running it, so callers that run the same
/// line over and over (function bodies) keep one of these per line.
pub struct CachedParse<S: CommandSource> {
    input: Arc<str>,
    /// Contexts that only modify the source, in the order they run.
    modifiers: Vec<CachedStage<S>>,
    /// The context whose command runs last.
    execute: CachedStage<S>,
}

impl<S: CommandSource> CachedParse<S> {
    /// Compiles the parse into the chain of stages that [`Self::execute`] runs.
    ///
    /// A parse that ends without an executable command keeps an empty `execute`
    /// stage, so running it reports the same failure as parsing did.
    #[must_use]
    pub fn from_builder(builder: CommandContextBuilder<'_, S>, input: Arc<str>) -> Self {
        let mut modifiers = Vec::new();
        let mut current = builder;
        loop {
            let CommandContextBuilder {
                arguments,
                root,
                nodes,
                range,
                child,
                modifier,
                forks,
                command,
                ..
            } = current;
            let stage = CachedStage {
                arguments: Arc::new(arguments),
                root,
                nodes: nodes.into(),
                range,
                modifier,
                forks,
                command,
            };
            match child {
                Some(child) => {
                    modifiers.push(stage);
                    current = *child;
                }
                None => {
                    return Self {
                        input,
                        modifiers,
                        execute: stage,
                    };
                }
            }
        }
    }

    /// Rebuilds the executable context for `source`.
    fn build<'a>(
        &self,
        tree: &'a Tree<S>,
        stage: &CachedStage<S>,
        source: &Arc<S>,
    ) -> CommandContext<'a, S> {
        CommandContext {
            source: source.clone(),
            input: self.input.clone(),
            arguments: stage.arguments.clone(),
            tree,
            root: stage.root,
            nodes: stage.nodes.clone(),
            range: stage.range,
            child: None,
            modifier: stage.modifier.clone(),
            forks: stage.forks,
            command: stage.command.clone(),
        }
    }

    /// Runs the compiled line with `source`.
    ///
    /// Mirrors vanilla's `CommandDispatcher.execute`: modifier stages fork the
    /// source, the last stage runs the command, and the consumer sees one
    /// completion per executed command unless the command suppressed it.
    pub fn execute(
        &self,
        tree: &Tree<S>,
        source: &Arc<S>,
        consumer: &dyn ResultConsumer<S>,
    ) -> Result<i32, CommandSyntaxError> {
        if self.execute.command.is_none() {
            let context = self.build(tree, &self.execute, source);
            consumer.on_command_completion(&context, ReturnValue::Failure)?;
            return Err(CommandDispatcher::<S>::unknown_command_error_for(
                &self.input,
            ));
        }

        if self.modifiers.is_empty() {
            return self.run_executable(tree, &self.execute, source, consumer, false);
        }

        let mut forked_mode = false;
        let mut current_sources = vec![source.clone()];

        for stage in &self.modifiers {
            forked_mode |= stage.forks;

            let mut next_sources = Vec::with_capacity(current_sources.len());
            for current_source in &current_sources {
                // `execute run` and other plain continuations never read the
                // context, so skip rebuilding it for every source.
                if matches!(stage.modifier, RedirectModifier::KeepSource) {
                    next_sources.push(current_source.clone());
                    continue;
                }
                let context = self.build(tree, stage, current_source);
                match stage.modifier.sources(&context) {
                    Ok(mut sources) => next_sources.append(&mut sources),
                    // A failing modifier only ends the line when it does not fork.
                    Err(error) if !forked_mode => return Err(error),
                    Err(_) => {}
                }
            }
            if next_sources.is_empty() {
                return Ok(0);
            }
            current_sources = next_sources;
        }

        let mut result = 0;
        for execution_source in current_sources {
            result += self.run_executable(
                tree,
                &self.execute,
                &execution_source,
                consumer,
                forked_mode,
            )?;
        }

        Ok(result)
    }

    fn run_executable(
        &self,
        tree: &Tree<S>,
        stage: &CachedStage<S>,
        source: &Arc<S>,
        consumer: &dyn ResultConsumer<S>,
        forked_mode: bool,
    ) -> Result<i32, CommandSyntaxError> {
        let Some(command) = stage.command.as_ref() else {
            return Ok(0);
        };
        let context = self.build(tree, stage, source);
        let result = command.execute(&context);

        if let Ok(value) = result {
            // Vanilla's `function` only reports a result when the function used `return`.
            if !take_skip_result() {
                consumer.on_command_completion(&context, ReturnValue::Success(value))?;
            }
            Ok(if forked_mode { 1 } else { value })
        } else {
            consumer.on_command_completion(&context, ReturnValue::Failure)?;
            if forked_mode { Ok(0) } else { result }
        }
    }
}

/// Successful parses for one permission key, keyed by the input line.
type CachedLines<S> = FxHashMap<Box<str>, Arc<CachedParse<S>>>;

/// Successful parses keyed by a caller-chosen permission key and the input line.
///
/// Parsing can depend on the source's permissions (entity selectors), so the
/// caller passes a key that groups sources with identical permissions.
/// Cloning a dispatcher starts an empty cache, so a changed command tree never
/// sees parses made against the old one.
pub struct ParseCache<S: CommandSource> {
    entries: RwLock<FxHashMap<u8, CachedLines<S>>>,
}

/// Upper bound on cached lines, so unexpected unique inputs cannot grow it forever.
const MAX_CACHED_PARSES: usize = 65_536;

impl<S: CommandSource> ParseCache<S> {
    #[must_use]
    pub fn get(&self, key: u8, input: &str) -> Option<Arc<CachedParse<S>>> {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.get(&key)?.get(input).cloned()
    }

    pub fn insert(&self, key: u8, input: &str, parsed: CachedParse<S>) -> Arc<CachedParse<S>> {
        let parsed = Arc::new(parsed);
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lines = entries.entry(key).or_default();
        if lines.len() >= MAX_CACHED_PARSES {
            lines.clear();
        }
        lines.insert(input.into(), parsed.clone());
        parsed
    }

    pub fn clear(&mut self) {
        self.entries
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

impl<S: CommandSource> Default for ParseCache<S> {
    fn default() -> Self {
        Self {
            entries: RwLock::new(FxHashMap::default()),
        }
    }
}

impl<S: CommandSource> Clone for ParseCache<S> {
    fn clone(&self) -> Self {
        Self::default()
    }
}
