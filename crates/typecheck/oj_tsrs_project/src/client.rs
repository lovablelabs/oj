use std::fmt::Display;

use tsrs_core::context::{Context, Locale};
use tsrs_diagnostics::Message;
use tsrs_lsproto as lsproto;

use crate::watch::WatcherID;

// client.go:11
// Go's variadic `args ...any` message arguments are `&[&dyn Display]` (as in diagnostics formatting); Go `error`
// results are `lsproto::Error`.
pub trait Client: Send + Sync {
    fn watch_files(
        &self,
        ctx: &Context,
        id: WatcherID,
        watchers: Vec<lsproto::FileSystemWatcher>,
    ) -> Result<(), lsproto::Error>;
    fn unwatch_files(&self, ctx: &Context, id: WatcherID) -> Result<(), lsproto::Error>;
    fn register_content_mapper_extensions(
        &self,
        ctx: &Context,
        extensions: Vec<String>,
    ) -> Result<(), lsproto::Error>;
    fn refresh_diagnostics(&self, ctx: &Context) -> Result<(), lsproto::Error>;
    fn publish_diagnostics(
        &self,
        ctx: &Context,
        params: lsproto::PublishDiagnosticsParams,
    ) -> Result<(), lsproto::Error>;
    fn refresh_inlay_hints(&self, ctx: &Context) -> Result<(), lsproto::Error>;
    fn refresh_code_lens(&self, ctx: &Context) -> Result<(), lsproto::Error>;
    fn progress_start(&self, message: &'static Message, args: &[&dyn Display]);
    fn progress_finish(&self, message: &'static Message, args: &[&dyn Display]);
    fn send_telemetry(
        &self,
        ctx: &Context,
        telemetry: lsproto::TelemetryEvent,
    ) -> Result<(), lsproto::Error>;
    fn is_active(&self) -> bool;
    // SetLocale updates the locale used for diagnostic messages.
    fn set_locale(&self, locale: &str);
    // GetLocale returns the current display locale for diagnostic messages.
    fn get_locale(&self) -> Locale;
}
