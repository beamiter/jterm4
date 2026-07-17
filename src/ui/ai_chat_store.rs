//! Pure multi-chat runtime state for the AI panel.
//!
//! GTK owns one transcript and composer, while this store owns every chat's
//! provider history, Block context, draft, archive state and request token.
//! Request completion is keyed by `(chat_id, epoch)`, so background replies
//! cannot cross into the chat the user happens to be viewing.

use crate::ai::{
    BlockContext, ChatSnapshot, ConversationSnapshot, Role, Turn, MAX_PERSISTED_CHATS,
};

pub(super) const DEFAULT_CHAT_TITLE: &str = "New chat";
const MAX_CHAT_TITLE_BYTES: usize = 256;
const MAX_CHAT_TITLE_CHARS: usize = 80;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum ChatStatus {
    #[default]
    Idle,
    Thinking(String),
    Info(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RequestToken {
    pub(super) chat_id: u64,
    pub(super) epoch: u64,
}

#[derive(Clone, Debug)]
pub(super) struct RequestStart {
    pub(super) token: RequestToken,
    pub(super) history: Vec<Turn>,
    pub(super) effective_context: Option<BlockContext>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ChatSummary {
    pub(super) id: u64,
    pub(super) title: String,
    pub(super) preview: String,
    pub(super) archived: bool,
    pub(super) active: bool,
    pub(super) busy: bool,
    pub(super) unread: bool,
    pub(super) history_truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChatStoreError {
    LimitReached,
    Archived,
    Busy,
    EmptyMessage,
    SnapshotInvalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ArchiveOutcome {
    pub(super) archived: bool,
    pub(super) active_chat_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DeleteOutcome {
    pub(super) deleted_chat_id: u64,
    pub(super) active_chat_id: u64,
}

#[derive(Clone, Debug)]
struct ChatRuntime {
    id: u64,
    title: String,
    archived: bool,
    history: Vec<Turn>,
    block_context: Option<BlockContext>,
    draft: String,
    history_truncated: bool,
    epoch: u64,
    active_epoch: Option<u64>,
    pending_user: Option<String>,
    restore_pending_as_draft: bool,
    /// `None` means the active request did not replace context. `Some` keeps
    /// the last durable context (which can itself be absent) until success.
    previous_context: Option<Option<BlockContext>>,
    status: ChatStatus,
    unread: bool,
}

impl ChatRuntime {
    fn empty(id: u64) -> Self {
        Self {
            id,
            title: DEFAULT_CHAT_TITLE.to_string(),
            archived: false,
            history: Vec::new(),
            block_context: None,
            draft: String::new(),
            history_truncated: false,
            epoch: 0,
            active_epoch: None,
            pending_user: None,
            restore_pending_as_draft: false,
            previous_context: None,
            status: ChatStatus::Idle,
            unread: false,
        }
    }

    fn from_snapshot(snapshot: ChatSnapshot) -> Self {
        let (id, title, archived, history, block_context, draft, history_truncated) =
            snapshot.into_parts();
        Self {
            id,
            title,
            archived,
            history,
            block_context,
            draft,
            history_truncated,
            epoch: 0,
            active_epoch: None,
            pending_user: None,
            restore_pending_as_draft: false,
            previous_context: None,
            status: ChatStatus::Idle,
            unread: false,
        }
    }

    fn is_busy(&self) -> bool {
        self.active_epoch.is_some()
    }
}

#[derive(Clone, Debug)]
pub(super) struct ChatStore {
    /// Oldest to newest. Persistence compacts payload from the front while the
    /// library presents this vector in reverse order.
    chats: Vec<ChatRuntime>,
    active_chat_id: u64,
    next_id: u64,
}

impl Default for ChatStore {
    fn default() -> Self {
        Self {
            chats: vec![ChatRuntime::empty(1)],
            active_chat_id: 1,
            next_id: 2,
        }
    }
}

impl ChatStore {
    pub(super) fn restore(snapshot: ConversationSnapshot) -> Self {
        let (active_chat_id, snapshots) = snapshot.into_parts();
        let chats: Vec<_> = snapshots
            .into_iter()
            .map(ChatRuntime::from_snapshot)
            .collect();
        let next_id = next_available_id(&chats);
        let mut store = Self {
            chats,
            active_chat_id,
            next_id,
        };
        store.active_mut().unread = false;
        store
    }

    pub(super) fn snapshot_for_persistence(
        &mut self,
        redact: bool,
    ) -> Result<(ConversationSnapshot, bool), ChatStoreError> {
        let chats = self
            .chats
            .iter()
            .map(|chat| {
                let mut title = chat.title.clone();
                let mut history = chat.history.clone();
                let mut context = durable_context(chat);
                let mut draft = durable_draft(chat);
                if redact {
                    title = crate::redact::redact_secrets(&title);
                    draft = crate::redact::redact_secrets(&draft);
                    for turn in &mut history {
                        turn.text = crate::redact::redact_secrets(&turn.text);
                    }
                    if let Some(context) = context.as_mut() {
                        context.cmd = crate::redact::redact_secrets(&context.cmd);
                        context.output = crate::redact::redact_secrets(&context.output);
                        context.cwd = context
                            .cwd
                            .take()
                            .map(|cwd| crate::redact::redact_secrets(&cwd));
                    }
                }
                ChatSnapshot::from_completed_history(
                    chat.id,
                    &title,
                    chat.archived,
                    &history,
                    context.as_ref(),
                    &draft,
                )
                .with_history_truncated(chat.history_truncated)
            })
            .collect();
        let snapshot = ConversationSnapshot::from_chats(self.active_chat_id, chats)
            .ok_or(ChatStoreError::SnapshotInvalid)?;
        let mut truncation_changed = false;
        for persisted in snapshot.chats() {
            if !persisted.history_truncated() {
                continue;
            }
            if let Some(chat) = self.chat_mut(persisted.id()) {
                truncation_changed |= !chat.history_truncated;
                chat.history_truncated = true;
            }
        }
        Ok((snapshot, truncation_changed))
    }

    pub(super) fn sync_truncation_markers(&mut self, snapshot: &ConversationSnapshot) -> bool {
        let mut changed = false;
        for persisted in snapshot.chats() {
            if !persisted.history_truncated() {
                continue;
            }
            if let Some(chat) = self.chat_mut(persisted.id()) {
                changed |= !chat.history_truncated;
                chat.history_truncated = true;
            }
        }
        changed
    }

    pub(super) fn active_id(&self) -> u64 {
        self.active_chat_id
    }

    pub(super) fn active_title(&self) -> &str {
        &self.active().title
    }

    pub(super) fn active_archived(&self) -> bool {
        self.active().archived
    }

    pub(super) fn active_history(&self) -> &[Turn] {
        &self.active().history
    }

    pub(super) fn active_context(&self) -> Option<&BlockContext> {
        self.active().block_context.as_ref()
    }

    pub(super) fn active_draft(&self) -> &str {
        &self.active().draft
    }

    pub(super) fn active_status(&self) -> &ChatStatus {
        &self.active().status
    }

    pub(super) fn active_history_truncated(&self) -> bool {
        self.active().history_truncated
    }

    pub(super) fn is_active_busy(&self) -> bool {
        self.active().is_busy()
    }

    pub(super) fn len(&self) -> usize {
        self.chats.len()
    }

    pub(super) fn at_capacity(&self) -> bool {
        self.chats.len() >= MAX_PERSISTED_CHATS
    }

    pub(super) fn set_active_draft(&mut self, draft: String) -> bool {
        if self.active().draft == draft {
            return false;
        }
        self.active_mut().draft = draft;
        true
    }

    pub(super) fn new_chat(&mut self) -> Result<u64, ChatStoreError> {
        if self.at_capacity() {
            return Err(ChatStoreError::LimitReached);
        }
        let id = self.allocate_id();
        self.chats.push(ChatRuntime::empty(id));
        self.active_chat_id = id;
        Ok(id)
    }

    pub(super) fn select_chat(&mut self, id: u64) -> bool {
        if self.active_chat_id == id || !self.chats.iter().any(|chat| chat.id == id) {
            return false;
        }
        self.active_chat_id = id;
        self.active_mut().unread = false;
        true
    }

    pub(super) fn rename_active(&mut self, title: &str) -> bool {
        let title = normalise_title(title);
        if self.active().title == title {
            return false;
        }
        self.active_mut().title = title;
        true
    }

    pub(super) fn toggle_archive_active(&mut self) -> Result<ArchiveOutcome, ChatStoreError> {
        if self.active().archived {
            self.active_mut().archived = false;
            return Ok(ArchiveOutcome {
                archived: false,
                active_chat_id: self.active_chat_id,
            });
        }

        let archived_id = self.active_chat_id;
        self.active_mut().archived = true;
        if let Some(replacement) = self
            .chats
            .iter()
            .rev()
            .find(|chat| chat.id != archived_id && !chat.archived)
            .map(|chat| chat.id)
        {
            self.active_chat_id = replacement;
            self.active_mut().unread = false;
        } else if !self.at_capacity() {
            self.new_chat()?;
        }
        Ok(ArchiveOutcome {
            archived: true,
            active_chat_id: self.active_chat_id,
        })
    }

    pub(super) fn delete_active(&mut self) -> DeleteOutcome {
        let deleted_chat_id = self.active_chat_id;
        self.chats.retain(|chat| chat.id != deleted_chat_id);

        if let Some(replacement) = self
            .chats
            .iter()
            .rev()
            .find(|chat| !chat.archived)
            .map(|chat| chat.id)
        {
            self.active_chat_id = replacement;
            self.active_mut().unread = false;
        } else {
            // Deletion always frees one slot, so a writable replacement is
            // guaranteed even if every surviving chat is archived.
            let id = self.allocate_id();
            self.chats.push(ChatRuntime::empty(id));
            self.active_chat_id = id;
        }

        DeleteOutcome {
            deleted_chat_id,
            active_chat_id: self.active_chat_id,
        }
    }

    pub(super) fn begin_turn(
        &mut self,
        text: String,
        context: Option<BlockContext>,
        thinking_message: String,
        restore_pending_as_draft: bool,
    ) -> Result<RequestStart, ChatStoreError> {
        if text.trim().is_empty() {
            return Err(ChatStoreError::EmptyMessage);
        }
        if self.active().archived {
            return Err(ChatStoreError::Archived);
        }
        if self.active().is_busy() {
            return Err(ChatStoreError::Busy);
        }

        let chat = self.active_mut();
        chat.previous_context = context.as_ref().map(|_| chat.block_context.clone());
        if let Some(context) = context {
            chat.block_context = Some(context);
        }
        let effective_context = chat.block_context.clone();
        if chat.title == DEFAULT_CHAT_TITLE && chat.history.is_empty() {
            chat.title = title_from_text(&text);
        }
        chat.epoch = chat.epoch.wrapping_add(1);
        let token = RequestToken {
            chat_id: chat.id,
            epoch: chat.epoch,
        };
        chat.history.push(Turn {
            role: Role::User,
            text: text.clone(),
        });
        chat.active_epoch = Some(token.epoch);
        chat.pending_user = Some(text);
        chat.restore_pending_as_draft = restore_pending_as_draft;
        chat.status = ChatStatus::Thinking(thinking_message);
        chat.unread = false;

        Ok(RequestStart {
            token,
            history: chat.history.clone(),
            effective_context,
        })
    }

    /// Returns whether the owner chat is still the visible chat.
    pub(super) fn complete_success(&mut self, token: RequestToken, text: String) -> Option<bool> {
        let active_chat_id = self.active_chat_id;
        let chat = self.chat_mut(token.chat_id)?;
        if chat.active_epoch != Some(token.epoch) {
            return None;
        }
        chat.active_epoch = None;
        chat.pending_user = None;
        chat.restore_pending_as_draft = false;
        chat.previous_context = None;
        chat.history.push(Turn {
            role: Role::Assistant,
            text,
        });
        chat.status = ChatStatus::Idle;
        chat.unread = chat.id != active_chat_id;
        Some(chat.id == active_chat_id)
    }

    /// Roll back only the request owner's trailing user turn.
    pub(super) fn complete_error(&mut self, token: RequestToken, message: String) -> Option<bool> {
        let active_chat_id = self.active_chat_id;
        let chat = self.chat_mut(token.chat_id)?;
        if chat.active_epoch != Some(token.epoch) {
            return None;
        }
        chat.active_epoch = None;
        let popped_user = if chat
            .history
            .last()
            .is_some_and(|turn| turn.role == Role::User)
        {
            chat.history.pop().map(|turn| turn.text)
        } else {
            None
        };
        let pending_user = chat.pending_user.take().or(popped_user);
        if chat.restore_pending_as_draft {
            if let Some(pending_user) = pending_user {
                chat.draft = merge_drafts(&pending_user, &chat.draft);
            }
        }
        chat.restore_pending_as_draft = false;
        if let Some(previous_context) = chat.previous_context.take() {
            chat.block_context = previous_context;
        }
        chat.status = ChatStatus::Error(message);
        chat.unread = chat.id != active_chat_id;
        Some(chat.id == active_chat_id)
    }

    pub(super) fn set_active_info(&mut self, message: impl Into<String>) {
        self.active_mut().status = ChatStatus::Info(message.into());
    }

    pub(super) fn set_active_error(&mut self, message: impl Into<String>) {
        self.active_mut().status = ChatStatus::Error(message.into());
    }

    pub(super) fn clear_active_status(&mut self) {
        if !self.active().is_busy() {
            self.active_mut().status = ChatStatus::Idle;
        }
    }

    pub(super) fn summaries(&self) -> Vec<ChatSummary> {
        self.chats
            .iter()
            .rev()
            .map(|chat| ChatSummary {
                id: chat.id,
                title: chat.title.clone(),
                preview: chat_preview(chat),
                archived: chat.archived,
                active: chat.id == self.active_chat_id,
                busy: chat.is_busy(),
                unread: chat.unread,
                history_truncated: chat.history_truncated,
            })
            .collect()
    }

    fn active(&self) -> &ChatRuntime {
        self.chats
            .iter()
            .find(|chat| chat.id == self.active_chat_id)
            .expect("active chat invariant")
    }

    fn active_mut(&mut self) -> &mut ChatRuntime {
        let id = self.active_chat_id;
        self.chat_mut(id).expect("active chat invariant")
    }

    fn chat_mut(&mut self, id: u64) -> Option<&mut ChatRuntime> {
        self.chats.iter_mut().find(|chat| chat.id == id)
    }

    fn allocate_id(&mut self) -> u64 {
        let mut candidate = self.next_id.max(1);
        while self.chats.iter().any(|chat| chat.id == candidate) {
            candidate = candidate.wrapping_add(1).max(1);
        }
        self.next_id = candidate.wrapping_add(1).max(1);
        candidate
    }
}

fn next_available_id(chats: &[ChatRuntime]) -> u64 {
    let mut candidate = chats
        .iter()
        .map(|chat| chat.id)
        .max()
        .unwrap_or(0)
        .wrapping_add(1)
        .max(1);
    while chats.iter().any(|chat| chat.id == candidate) {
        candidate = candidate.wrapping_add(1).max(1);
    }
    candidate
}

fn normalise_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        DEFAULT_CHAT_TITLE.to_string()
    } else {
        let mut bounded = String::new();
        for ch in collapsed.chars().take(MAX_CHAT_TITLE_CHARS) {
            if bounded.len().saturating_add(ch.len_utf8()) > MAX_CHAT_TITLE_BYTES {
                break;
            }
            bounded.push(ch);
        }
        if bounded.is_empty() {
            DEFAULT_CHAT_TITLE.to_string()
        } else {
            bounded
        }
    }
}

fn title_from_text(text: &str) -> String {
    let collapsed = normalise_title(text);
    let mut chars = collapsed.chars();
    let title: String = chars.by_ref().take(52).collect();
    if chars.next().is_some() {
        format!("{title}…")
    } else {
        title
    }
}

fn chat_preview(chat: &ChatRuntime) -> String {
    let source = chat
        .history
        .last()
        .map(|turn| turn.text.as_str())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| (!chat.draft.trim().is_empty()).then_some(chat.draft.as_str()))
        .unwrap_or("Empty conversation");
    let collapsed = source.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let preview: String = chars.by_ref().take(72).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn durable_draft(chat: &ChatRuntime) -> String {
    if chat.restore_pending_as_draft {
        if let Some(pending_user) = chat.pending_user.as_deref() {
            return merge_drafts(pending_user, &chat.draft);
        }
    }
    chat.draft.clone()
}

fn durable_context(chat: &ChatRuntime) -> Option<BlockContext> {
    chat.previous_context
        .as_ref()
        .cloned()
        .unwrap_or_else(|| chat.block_context.clone())
}

fn merge_drafts(first: &str, second: &str) -> String {
    if first.is_empty() || first == second {
        return second.to_string();
    }
    if second.is_empty() {
        return first.to_string();
    }
    format!("{first}\n\n{second}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(store: &mut ChatStore, text: &str) -> RequestToken {
        store
            .begin_turn(text.into(), None, "Thinking…".into(), true)
            .unwrap()
            .token
    }

    fn finish(store: &mut ChatStore, text: &str, answer: &str) {
        let token = start(store, text);
        assert_eq!(store.complete_success(token, answer.into()), Some(true));
    }

    #[test]
    fn new_chat_preserves_old_history_and_selects_unique_empty_chat() {
        let mut store = ChatStore::default();
        finish(&mut store, "first question", "first answer");
        let old_id = store.active_id();
        let new_id = store.new_chat().unwrap();

        assert_ne!(new_id, old_id);
        assert_eq!(store.active_id(), new_id);
        assert!(store.active_history().is_empty());
        assert!(store.select_chat(old_id));
        assert_eq!(store.active_history().len(), 2);
    }

    #[test]
    fn switching_preserves_per_chat_drafts_and_context() {
        let mut store = ChatStore::default();
        let first_context = BlockContext {
            cmd: "cargo test".into(),
            output: "first output".into(),
            cwd: Some("/tmp/first".into()),
            exit_code: 0,
        };
        let first_token = store
            .begin_turn(
                "first question".into(),
                Some(first_context),
                "Thinking…".into(),
                true,
            )
            .unwrap()
            .token;
        store.complete_success(first_token, "first answer".into());
        store.set_active_draft("draft one".into());
        let first = store.active_id();
        let second = store.new_chat().unwrap();
        let second_context = BlockContext {
            cmd: "cargo clippy".into(),
            output: "second output".into(),
            cwd: Some("/tmp/second".into()),
            exit_code: 1,
        };
        let second_token = store
            .begin_turn(
                "second question".into(),
                Some(second_context),
                "Thinking…".into(),
                true,
            )
            .unwrap()
            .token;
        store.complete_success(second_token, "second answer".into());
        store.set_active_draft("draft two".into());

        assert!(store.select_chat(first));
        assert_eq!(store.active_draft(), "draft one");
        assert_eq!(store.active_context().unwrap().cmd, "cargo test");
        assert!(store.select_chat(second));
        assert_eq!(store.active_draft(), "draft two");
        assert_eq!(store.active_context().unwrap().cmd, "cargo clippy");
    }

    #[test]
    fn background_replies_and_errors_update_only_the_request_owner() {
        let mut store = ChatStore::default();
        let first = store.active_id();
        let first_token = start(&mut store, "question one");
        let second = store.new_chat().unwrap();
        let second_token = start(&mut store, "question two");

        assert_eq!(
            store.complete_success(first_token, "answer one".into()),
            Some(false)
        );
        assert_eq!(store.active_id(), second);
        assert_eq!(store.active_history().len(), 1);
        assert_eq!(
            store.complete_error(second_token, "failed".into()),
            Some(true)
        );
        assert!(store.active_history().is_empty());
        assert!(store.select_chat(first));
        assert_eq!(store.active_history().len(), 2);
        assert_eq!(store.active_history()[1].text, "answer one");
    }

    #[test]
    fn failed_and_inflight_messages_are_recoverable_as_drafts() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "please retry this");

        let (inflight, _) = store.snapshot_for_persistence(false).unwrap();
        assert!(inflight.active_chat().unwrap().turns().is_empty());
        assert_eq!(inflight.active_chat().unwrap().draft(), "please retry this");

        store.set_active_draft("follow-up notes".into());
        assert_eq!(
            store.complete_error(token, "network failed".into()),
            Some(true)
        );
        assert!(store.active_history().is_empty());
        assert_eq!(store.active_draft(), "please retry this\n\nfollow-up notes");
    }

    #[test]
    fn independent_block_request_preserves_the_existing_composer_draft() {
        let mut store = ChatStore::default();
        store.set_active_draft("my unrelated draft".into());
        let token = store
            .begin_turn(
                "Explain the selected Block".into(),
                None,
                "Thinking…".into(),
                false,
            )
            .unwrap()
            .token;

        let (inflight, _) = store.snapshot_for_persistence(false).unwrap();
        assert_eq!(
            inflight.active_chat().unwrap().draft(),
            "my unrelated draft"
        );
        store.complete_error(token, "network failed".into());
        assert_eq!(store.active_draft(), "my unrelated draft");
    }

    #[test]
    fn replacement_context_becomes_durable_only_after_success() {
        let mut store = ChatStore::default();
        let old_context = BlockContext {
            cmd: "cargo test".into(),
            output: "old output".into(),
            cwd: Some("/tmp/old".into()),
            exit_code: 0,
        };
        let old_request = store
            .begin_turn(
                "explain the old block".into(),
                Some(old_context.clone()),
                "Thinking…".into(),
                false,
            )
            .unwrap();
        store.complete_success(old_request.token, "old answer".into());

        let new_context = BlockContext {
            cmd: "cargo clippy".into(),
            output: "new output".into(),
            cwd: Some("/tmp/new".into()),
            exit_code: 1,
        };
        let replacement = store
            .begin_turn(
                "explain the new block".into(),
                Some(new_context.clone()),
                "Thinking…".into(),
                false,
            )
            .unwrap();

        assert_eq!(replacement.effective_context.as_ref(), Some(&new_context));
        assert_eq!(store.active_context(), Some(&new_context));
        let (inflight, _) = store.snapshot_for_persistence(false).unwrap();
        assert_eq!(inflight.active_chat().unwrap().turns().len(), 2);
        assert_eq!(inflight.block_context(), Some(&old_context));

        store.complete_error(replacement.token, "network failed".into());
        assert_eq!(store.active_history().len(), 2);
        assert_eq!(store.active_context(), Some(&old_context));
        let (failed, _) = store.snapshot_for_persistence(false).unwrap();
        assert_eq!(failed.block_context(), Some(&old_context));
    }

    #[test]
    fn failed_first_context_request_does_not_leave_orphan_context() {
        let mut store = ChatStore::default();
        let request = store
            .begin_turn(
                "explain this block".into(),
                Some(BlockContext {
                    cmd: "false".into(),
                    output: "failed".into(),
                    cwd: None,
                    exit_code: 1,
                }),
                "Thinking…".into(),
                false,
            )
            .unwrap();

        store.complete_error(request.token, "network failed".into());
        assert!(store.active_context().is_none());
        let (snapshot, _) = store.snapshot_for_persistence(false).unwrap();
        assert!(snapshot.block_context().is_none());
    }

    #[test]
    fn successful_request_keeps_a_follow_up_draft_typed_while_busy() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "first question");
        store.set_active_draft("next question".into());

        store.complete_success(token, "first answer".into());
        assert_eq!(store.active_draft(), "next question");
    }

    #[test]
    fn deleting_an_inflight_chat_makes_late_completion_a_noop() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "will be deleted");
        let deleted = store.delete_active();
        assert_eq!(deleted.deleted_chat_id, token.chat_id);
        assert_eq!(store.complete_success(token, "late".into()), None);
        assert!(store.active_history().is_empty());
    }

    #[test]
    fn archive_preserves_chat_and_selects_a_writable_replacement() {
        let mut store = ChatStore::default();
        finish(&mut store, "keep me", "kept");
        let archived = store.active_id();
        let outcome = store.toggle_archive_active().unwrap();

        assert!(outcome.archived);
        assert_ne!(outcome.active_chat_id, archived);
        let summary = store
            .summaries()
            .into_iter()
            .find(|summary| summary.id == archived)
            .unwrap();
        assert!(summary.archived);
        assert!(store.select_chat(archived));
        assert!(store.active_archived());
        assert!(!store.toggle_archive_active().unwrap().archived);
    }

    #[test]
    fn empty_active_archived_chat_and_drafts_round_trip() {
        let mut store = ChatStore::default();
        finish(&mut store, "old", "answer");
        store.toggle_archive_active().unwrap();
        store.set_active_draft("unfinished draft".into());
        let selected = store.active_id();

        let (snapshot, _) = store.snapshot_for_persistence(false).unwrap();
        let restored = ChatStore::restore(
            ConversationSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        );
        assert_eq!(restored.active_id(), selected);
        assert_eq!(restored.active_draft(), "unfinished draft");
        assert!(restored.summaries().iter().any(|chat| chat.archived));
    }

    #[test]
    fn persistence_redacts_every_chat_including_title_draft_and_context() {
        const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";
        let mut store = ChatStore::default();
        store.rename_active(&format!("title {SECRET}"));
        let context = BlockContext {
            cmd: format!("echo {SECRET}"),
            output: format!("output {SECRET}"),
            cwd: Some(format!("/tmp/{SECRET}")),
            exit_code: 0,
        };
        let start = store
            .begin_turn(
                format!("question {SECRET}"),
                Some(context),
                "Thinking…".into(),
                true,
            )
            .unwrap();
        store.complete_success(start.token, format!("answer {SECRET}"));
        store.set_active_draft(format!("archived draft {SECRET}"));
        let archived_id = store.active_id();

        let second_id = store.new_chat().unwrap();
        store.rename_active(&format!("second {SECRET}"));
        finish(
            &mut store,
            &format!("second question {SECRET}"),
            "second answer",
        );
        store.set_active_draft(format!("inactive draft {SECRET}"));

        assert!(store.select_chat(archived_id));
        store.toggle_archive_active().unwrap();
        assert_eq!(store.active_id(), second_id);

        store.new_chat().unwrap();
        store.rename_active(&format!("active {SECRET}"));
        store.set_active_draft(format!("active draft {SECRET}"));

        let (snapshot, _) = store.snapshot_for_persistence(true).unwrap();
        assert_eq!(snapshot.chats().len(), 3);
        assert!(snapshot.chats().iter().any(ChatSnapshot::archived));
        let json = snapshot.to_json().unwrap();
        assert!(!json.contains(SECRET));
        assert!(json.contains("REDACTED"));
    }

    #[test]
    fn chat_limit_refuses_new_chat_without_deleting_existing_rows() {
        let mut store = ChatStore::default();
        while store.len() < MAX_PERSISTED_CHATS {
            store.new_chat().unwrap();
        }
        let before = store.summaries();
        assert_eq!(store.new_chat(), Err(ChatStoreError::LimitReached));
        assert_eq!(store.summaries(), before);
    }

    #[test]
    fn runtime_title_matches_persistence_limits() {
        let mut store = ChatStore::default();
        store.rename_active(&format!("bad\0title {}", "😀".repeat(100)));

        assert!(!store.active_title().chars().any(char::is_control));
        assert!(store.active_title().len() <= MAX_CHAT_TITLE_BYTES);
        assert!(store.active_title().chars().count() <= MAX_CHAT_TITLE_CHARS);
        let expected = store.active_title().to_string();
        let (snapshot, _) = store.snapshot_for_persistence(false).unwrap();
        assert_eq!(snapshot.active_chat().unwrap().title(), expected);
    }

    #[test]
    fn persistence_compaction_marks_the_live_chat_immediately() {
        let mut store = ChatStore::default();
        for index in 0..51 {
            finish(
                &mut store,
                &format!("question {index}"),
                &format!("answer {index}"),
            );
        }

        let (snapshot, changed) = store.snapshot_for_persistence(false).unwrap();
        assert!(changed);
        assert!(store.active_history_truncated());
        assert!(snapshot.active_chat().unwrap().history_truncated());
        assert_eq!(snapshot.active_chat().unwrap().turns().len(), 100);

        let (_, changed_again) = store.snapshot_for_persistence(false).unwrap();
        assert!(!changed_again);
    }

    #[test]
    fn state_level_compaction_marker_syncs_back_to_the_runtime_chat() {
        let mut store = ChatStore::default();
        finish(&mut store, "question", "answer");
        let (snapshot, _) = store.snapshot_for_persistence(false).unwrap();
        let active_id = snapshot.active_chat_id();
        let marked_chats = snapshot
            .chats()
            .iter()
            .cloned()
            .map(|chat| {
                let is_active = chat.id() == active_id;
                chat.with_history_truncated(is_active)
            })
            .collect();
        let marked = ConversationSnapshot::from_chats(active_id, marked_chats).unwrap();

        assert!(store.sync_truncation_markers(&marked));
        assert!(store.active_history_truncated());
        assert!(!store.sync_truncation_markers(&marked));
    }
}
