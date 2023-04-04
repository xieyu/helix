use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio;
// use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::{self, JoinHandle};
use tokio::time::timeout;

use helix_core::chars::char_is_word;

use crate::DocumentId;

// TODO get completion items len from config
const MAX_COMPLETION_ITEMS_LEN: usize = 20;
const TIMEOUT: Duration = Duration::from_millis(300);
const MAX_CONCURRENT_COMMANDS: usize = 10;
pub const CHANGED_LINES_TO_PROCESS_WHOLE_DOC: usize = 100;

pub struct WordsCompletion {
    tx: mpsc::Sender<CompletionRequest>,
    handle: JoinHandle<()>,
    is_stopped: Arc<AtomicBool>,
}

impl Default for WordsCompletion {
    fn default() -> Self {
        Self::new(2)
    }
}

struct WordsCompletionState {
    rx: mpsc::Receiver<CompletionRequest>,
    min_word_len: usize,

    // TODO limit hashmap?
    // words extracted on load/save document
    doc_words: HashMap<DocumentId, HashSet<String>>,

    // words extracted on document changes assiciated to concrete lines
    // on load/save document cleared
    doc_lines_words: HashMap<DocumentId, HashMap<usize, HashSet<String>>>,

    is_stopped: Arc<AtomicBool>,
}

pub enum CompletionRequest {
    ExtractDocWords {
        doc_id: DocumentId,
        text: String,
    },
    ExtractDocLineWords {
        doc_id: DocumentId,
        lines: Vec<(usize, Option<String>)>,
    },
    Completion {
        prefix: String,
        tx: oneshot::Sender<Option<Vec<String>>>,
    },
}

impl std::fmt::Debug for CompletionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompletionRequest::ExtractDocWords { doc_id, text } => f
                .debug_struct("ExtractDocWords")
                .field("doc_id", doc_id)
                .field("text_len", &text.len())
                .finish(),
            CompletionRequest::ExtractDocLineWords { doc_id, lines } => f
                .debug_struct("ExtractDocLineWords")
                .field("doc_id", doc_id)
                .field("lines_len", &lines.len())
                .finish(),
            CompletionRequest::Completion { prefix, tx: _ } => f
                .debug_struct("Completion")
                .field("prefix", prefix)
                .finish(),
        }
    }
}

#[inline]
fn text_to_words(text: &str, min_word_len: usize) -> HashSet<&str> {
    HashSet::from_iter(text.split_ascii_whitespace().flat_map(|item| {
        item.split_terminator(|c| !char_is_word(c))
            .filter(|word| word.len() >= min_word_len)
    }))
}

impl WordsCompletion {
    pub fn new(completion_trigger_len: u8) -> Self {
        let (command_tx, command_rx) = mpsc::channel::<CompletionRequest>(MAX_CONCURRENT_COMMANDS);

        let is_stopped = Arc::new(AtomicBool::new(false));
        let is_stopped_clone = is_stopped.clone();

        let min_word_len = completion_trigger_len as usize + 1;

        // Start on separate thread
        let handle = task::spawn_blocking(move || {
            log::debug!("Words completion. start");
            let mut state = WordsCompletionState {
                rx: command_rx,
                min_word_len,
                doc_words: HashMap::new(),
                doc_lines_words: HashMap::new(),
                is_stopped: is_stopped_clone,
            };

            state.process_commands();
        });

        WordsCompletion {
            tx: command_tx,
            handle,
            is_stopped,
        }
    }

    fn send(&self, cmd: CompletionRequest) {
        if self.is_stopped.load(Ordering::SeqCst) {
            return;
        }
        log::debug!("Words completion command: {:?}", cmd);
        match self.tx.try_send(cmd) {
            Err(mpsc::error::TrySendError::Closed(_)) => {
                log::debug!("Words completion commands channel is closed");
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::trace!("Words completion commands channel is full");
            }
            _ => {}
        };
    }

    pub fn stop(&self) {
        self.is_stopped.store(true, Ordering::SeqCst);
    }

    pub fn extract_words(&self, doc_id: DocumentId, text: String) {
        self.send(CompletionRequest::ExtractDocWords { doc_id, text });
    }

    pub fn extract_line_words(&self, doc_id: DocumentId, lines: Vec<(usize, Option<String>)>) {
        self.send(CompletionRequest::ExtractDocLineWords { doc_id, lines });
    }

    pub async fn completion(&self, prefix: String) -> Option<Vec<String>> {
        if self.is_stopped.load(Ordering::SeqCst) {
            return None;
        }

        log::trace!("Words completion command: completion {}", prefix,);

        // TODO pool of oneshot channels
        let (tx, rx) = oneshot::channel::<Option<Vec<String>>>();

        if let Err(e) = self
            .tx
            .send_timeout(CompletionRequest::Completion { prefix, tx }, TIMEOUT)
            .await
        {
            // MAX_CONCURRENT_COMMANDS reached (channel is full)
            // worker can't accept and proccess command in time less then TIMEOUT
            log::trace!("On send command: {}", e);
            return None;
        }

        match timeout(TIMEOUT, rx).await {
            Ok(r) => match r {
                Ok(items) => items,
                Err(e) => {
                    log::info!("On wait esult: {}", e);
                    None
                }
            },
            Err(e) => {
                log::info!("On wait result timeout: {}", e);
                None
            }
        }
    }
}

impl Drop for WordsCompletion {
    fn drop(&mut self) {
        self.stop();
        self.handle.abort();
    }
}

impl WordsCompletionState {
    fn process_commands(&mut self) {
        loop {
            if self.is_stopped.load(Ordering::SeqCst) {
                log::debug!("Words completion. Stop");
                return;
            }

            if let Some(command) = self.rx.blocking_recv() {
                let now = std::time::Instant::now();
                let cmd_debug = format!("{:?}", command);

                match command {
                    CompletionRequest::ExtractDocWords { doc_id, text } => {
                        self.process_text(doc_id, text);
                    }
                    CompletionRequest::ExtractDocLineWords { doc_id, lines } => {
                        self.process_line_text(doc_id, lines);
                    }
                    CompletionRequest::Completion { prefix, tx } => {
                        self.completion(prefix, tx);
                    }
                }
                log::debug!(
                    "Workds completion {} took {}ms",
                    cmd_debug,
                    now.elapsed().as_millis()
                );
            }
        }
    }

    fn process_text(&mut self, doc_id: DocumentId, text: String) {
        let new_words = text_to_words(&text, self.min_word_len);

        if !new_words.is_empty() {
            let new_words: HashSet<String> = new_words.into_iter().map(String::from).collect();
            self.doc_words.insert(doc_id, new_words);
        } else if let Some(words) = self.doc_words.get_mut(&doc_id) {
            words.clear();
        }

        // reset doc lines words
        if let Some(lines) = self.doc_lines_words.get_mut(&doc_id) {
            lines.clear();
        }
    }

    fn process_line_text(&mut self, doc_id: DocumentId, lines: Vec<(usize, Option<String>)>) {
        for (line, text) in lines {
            if let Some(text) = text {
                let new_words = text_to_words(&text, self.min_word_len);

                let doc_lines = self
                    .doc_lines_words
                    .entry(doc_id)
                    .or_insert_with(HashMap::new);

                if new_words.is_empty() {
                    if let Some(doc_lines_words) = doc_lines.get_mut(&line) {
                        doc_lines_words.clear();
                    }
                } else {
                    doc_lines.insert(line, new_words.into_iter().map(String::from).collect());
                }
            } else if let Some(lines) = self.doc_lines_words.get_mut(&doc_id) {
                lines.remove(&line);
            }
        }
    }

    fn completion<'a>(&'a mut self, prefix: String, tx: oneshot::Sender<Option<Vec<String>>>) {
        // TODO use some index to speedup filter?

        let filter = |word: &'a String| -> Option<&'a str> {
            if word != &prefix && word.starts_with(&prefix) {
                Some(word.as_str())
            } else {
                None
            }
        };

        // find in affected lines
        let items_by_lines = self
            .doc_lines_words
            .values()
            .flatten()
            .flat_map(|m| m.1)
            .filter_map(filter)
            .take(MAX_COMPLETION_ITEMS_LEN)
            .collect::<HashSet<&str>>();

        let result = if items_by_lines.len() == MAX_COMPLETION_ITEMS_LEN {
            Some(
                items_by_lines
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<String>>(),
            )
        } else {
            // find in docs
            let mut items = self
                .doc_words
                .values()
                .flatten()
                .filter_map(filter)
                .take(MAX_COMPLETION_ITEMS_LEN - items_by_lines.len())
                .collect::<HashSet<&str>>();

            if !items_by_lines.is_empty() {
                items.extend(items_by_lines);
            }

            if items.is_empty() {
                None
            } else {
                Some(items.into_iter().map(String::from).collect::<Vec<String>>())
            }
        };

        if tx.send(result).is_err() {
            log::info!("On send completion result");
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn completion() {
        use tokio::runtime::Runtime;

        // Create the runtime
        let rt = Runtime::new().unwrap();

        let doc_id = DocumentId::default();

        let _ = rt.block_on(async move {
            let instance = WordsCompletion::new(2);

            // buffer with text
            let _ = instance.extract_words(doc_id, "Hello".to_string());

            let items = instance.completion("H".to_string()).await;
            assert_eq!(items, Some(vec!["Hello".to_string()]));

            // add text to the same line
            let _ = instance.extract_line_words(doc_id, vec![(0, Some("Hello world".to_string()))]);

            let items = instance.completion("w".to_string()).await;
            assert_eq!(items, Some(vec!["world".to_string()]));

            // reload buffer with text
            let _ = instance.extract_words(doc_id, "Hello".to_string());

            let items = instance.completion("w".to_string()).await;
            assert_eq!(items, None);

            drop(instance);

            true
        });
    }

    #[test]
    fn to_words() {
        assert_eq!(
            text_to_words("Hello World!", 2),
            HashSet::from_iter(["Hello", "World"])
        );

        assert_eq!(
            text_to_words("crate::mod::func", 2),
            HashSet::from_iter(["crate", "mod", "func"])
        );

        assert_eq!(
            text_to_words("crate::mod::a", 2),
            HashSet::from_iter(["crate", "mod"])
        );

        assert_eq!(
            text_to_words("crate10::mod2mod::func3", 2),
            HashSet::from_iter(["crate10", "mod2mod", "func3"])
        );

        assert_eq!(
            text_to_words("1 22 333 4444", 2),
            HashSet::from_iter(["22", "333", "4444"])
        );
    }
}
