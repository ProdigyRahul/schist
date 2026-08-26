//! Plugin registries — the kernel's catalog of everything installed.

use crate::{CodecPlugin, Command, CommandPlugin, FilterPlugin, ToolPlugin};
use std::sync::Arc;

/// All registered plugins, assembled at startup by the app shell from each
/// enabled `PluginManifest`.
#[derive(Default)]
pub struct PluginRegistry {
    tools: Vec<Box<dyn ToolPlugin>>,
    /// `Arc`, not `Box`: decoding runs on background threads, which need
    /// to hold the codec beyond the registry borrow.
    codecs: Vec<Arc<dyn CodecPlugin>>,
    commands: Vec<Command>,
    filters: Vec<Box<dyn FilterPlugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_tool(&mut self, tool: Box<dyn ToolPlugin>) {
        debug_assert!(
            !self.tools.iter().any(|t| t.id() == tool.id()),
            "duplicate tool id {}",
            tool.id()
        );
        self.tools.push(tool);
    }

    /// Register a codec. A duplicate id is dropped with a warning rather
    /// than shadowing the first: `codec_for` takes the first match, so a
    /// second registration under the same id is dead weight that probes
    /// on every file open and can never be selected. Tools already
    /// assert on this.
    pub fn register_codec(&mut self, codec: Box<dyn CodecPlugin>) {
        if self.codecs.iter().any(|c| c.id() == codec.id()) {
            log::warn!("ignoring duplicate codec id {}", codec.id());
            return;
        }
        self.codecs.push(Arc::from(codec));
    }

    pub fn register_commands(&mut self, plugin: &dyn CommandPlugin) {
        for command in plugin.commands() {
            if self.commands.iter().any(|c| c.id == command.id) {
                log::warn!("ignoring duplicate command id {}", command.id);
                continue;
            }
            self.commands.push(command);
        }
    }

    /// Register a filter, dropping a duplicate id for the same reason as
    /// codecs: the menu would show two identical entries and the lookup
    /// would always reach the first.
    pub fn register_filter(&mut self, filter: Box<dyn FilterPlugin>) {
        if self.filters.iter().any(|f| f.id() == filter.id()) {
            log::warn!("ignoring duplicate filter id {}", filter.id());
            return;
        }
        self.filters.push(filter);
    }

    pub fn tools(&self) -> impl Iterator<Item = &dyn ToolPlugin> {
        self.tools.iter().map(|t| t.as_ref())
    }

    pub fn tool_mut(&mut self, id: &str) -> Option<&mut Box<dyn ToolPlugin>> {
        self.tools.iter_mut().find(|t| t.id() == id)
    }

    pub fn tool_ids(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.id()).collect()
    }

    pub fn codecs(&self) -> impl Iterator<Item = &dyn CodecPlugin> {
        self.codecs.iter().map(|c| c.as_ref())
    }

    /// Find a codec by sniffing bytes, falling back to file extension.
    pub fn codec_for(&self, bytes: &[u8], extension: Option<&str>) -> Option<&dyn CodecPlugin> {
        self.codecs
            .iter()
            .find(|c| c.probe(bytes))
            .or_else(|| {
                let ext = extension?.to_ascii_lowercase();
                self.codecs
                    .iter()
                    .find(|c| c.extensions().contains(&ext.as_str()))
            })
            .map(|c| c.as_ref())
    }

    /// Clones of every codec, for decoding on a background thread.
    pub fn shared_codecs(&self) -> Vec<Arc<dyn CodecPlugin>> {
        self.codecs.clone()
    }

    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    pub fn command(&self, id: &str) -> Option<&Command> {
        self.commands.iter().find(|c| c.id == id)
    }

    pub fn filters(&self) -> impl Iterator<Item = &dyn FilterPlugin> {
        self.filters.iter().map(|f| f.as_ref())
    }
}

/// One plugin crate's entry point.
pub trait PluginManifest {
    fn id(&self) -> &'static str;
    fn register(&self, registry: &mut PluginRegistry);
}

#[cfg(test)]
mod duplicate_id_tests {
    use super::*;
    use crate::FilterValues;

    struct Codec(&'static str);
    impl CodecPlugin for Codec {
        fn id(&self) -> &'static str {
            self.0
        }
        fn name(&self) -> &'static str {
            "Test"
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["tst"]
        }
        fn probe(&self, bytes: &[u8]) -> bool {
            bytes.starts_with(b"TST")
        }
        fn import(&self, _bytes: &[u8]) -> anyhow::Result<schist_core::Document> {
            anyhow::bail!("test codec")
        }
    }

    struct Filter(&'static str);
    impl FilterPlugin for Filter {
        fn id(&self) -> &'static str {
            self.0
        }
        fn name(&self) -> &'static str {
            "Test"
        }
        fn apply(&self, _p: &mut [f32], _w: usize, _h: usize, _v: &FilterValues) {}
    }

    /// A second registration under the same id can never be selected --
    /// `codec_for` and the filter lookup both take the first match -- so
    /// it is dead weight that still probes on every file open, and shows
    /// as a duplicate menu entry. Tools already assert on this.
    #[test]
    fn duplicate_ids_are_dropped_rather_than_shadowing() {
        let mut reg = PluginRegistry::new();
        reg.register_codec(Box::new(Codec("codec.test")));
        reg.register_codec(Box::new(Codec("codec.test")));
        assert_eq!(reg.codecs().filter(|c| c.id() == "codec.test").count(), 1);

        reg.register_filter(Box::new(Filter("filter.test")));
        reg.register_filter(Box::new(Filter("filter.test")));
        assert_eq!(reg.filters().filter(|f| f.id() == "filter.test").count(), 1);

        // A different id still registers.
        reg.register_codec(Box::new(Codec("codec.other")));
        assert_eq!(reg.codecs().count(), 2);
    }
}
