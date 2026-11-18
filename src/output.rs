//! Prompt-safe output for asynchronous server diagnostics.
//!
//! While reedline owns the terminal, output goes through its external printer
//! so the active input is cleared and redrawn. At startup, during a command,
//! and in `--exec` mode, diagnostics stay on stderr.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc::SyncSender};

use reedline::ExternalPrinter;

/// Shared destination for output that can arrive independently of a command.
#[derive(Clone)]
pub struct AsyncOutput {
    at_prompt: Arc<AtomicBool>,
    printer: Arc<Mutex<Option<ExternalPrinter<String>>>>,
    sender: Option<SyncSender<String>>,
}

impl AsyncOutput {
    /// Build a sink, with a reedline printer only for interactive sessions.
    pub fn new(at_prompt: Arc<AtomicBool>, interactive: bool) -> Self {
        let printer = interactive.then(|| ExternalPrinter::new(1024));
        let sender = printer.as_ref().map(ExternalPrinter::sender);
        Self {
            at_prompt,
            printer: Arc::new(Mutex::new(printer)),
            sender,
        }
    }

    /// Move the single receiver to the editor. Clones retain only its sender.
    pub fn external_printer(&self) -> Option<ExternalPrinter<String>> {
        self.printer.lock().unwrap().take()
    }

    /// Print one logical line without disturbing an active prompt.
    pub fn line(&self, line: impl Into<String>) {
        let line = line.into();
        if self.at_prompt.load(Ordering::SeqCst)
            && let Some(sender) = &self.sender
            && sender.try_send(line.clone()).is_ok()
        {
            return;
        }
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_prompt_uses_reedline_external_printer() {
        let at_prompt = Arc::new(AtomicBool::new(true));
        let output = AsyncOutput::new(at_prompt, true);

        output.line("server diagnostic");

        assert_eq!(
            output.external_printer().unwrap().get_line().as_deref(),
            Some("server diagnostic")
        );
    }

    #[test]
    fn cloned_senders_share_one_editor_owned_receiver() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AsyncOutput>();
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(true)), true);
        let clone = output.clone();
        let receiver = output.external_printer().unwrap();
        assert!(clone.external_printer().is_none());
        std::thread::spawn(move || clone.line("from worker"))
            .join()
            .unwrap();
        output.line("from session");
        assert_eq!(receiver.get_line().as_deref(), Some("from worker"));
        assert_eq!(receiver.get_line().as_deref(), Some("from session"));
    }

    #[test]
    fn non_interactive_output_has_no_printer() {
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(false)), false);
        assert!(output.external_printer().is_none());
    }
}
