//! Reducer of the failover groups editor: the draft section, its undo stack and the preview bookkeeping.

use super::draft::{DraftOp, GroupRef};
use shared::model::{
    FailoverGroupListDto, FailoverGroupSaveResultDto, TargetFailoverOverridesDto, TargetOverridesResponseDto,
};
use std::rc::Rc;
use yew::Reducible;

const MAX_UNDO_STEPS: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum FailoverGroupsEditorAction {
    /// The section as the server holds it; replaces the draft.
    Loaded(TargetOverridesResponseDto),
    /// One user action on the draft, a single undo step.
    Edit(DraftOp),
    Undo,
    /// A preview for a draft at least as new as `seq` is wanted; responses for older drafts become stale.
    PreviewRequested(u64),
    /// The preview of the draft that carried `seq`; ignored when the draft changed since.
    PreviewLoaded(u64, FailoverGroupListDto),
    /// The preview for the draft that carried `seq` could not be produced; the page stops waiting for it.
    PreviewFailed(u64),
    Select(Option<GroupRef>),
    /// A save request is on its way; change notifications are ignored until it is answered.
    SaveStarted,
    /// The save request was answered (stored, refused or failed).
    SaveFinished,
    /// The draft `saved` was stored; edits made since stay in the draft.
    Saved(FailoverGroupSaveResultDto, TargetFailoverOverridesDto),
    /// The server section changed since the draft was based on it (`current_etag`); the draft is kept.
    Conflict(Option<String>),
    /// Reverts the draft to the server section.
    Discard,
    /// The section on the server as read after a change notification.
    ExternalChange(TargetOverridesResponseDto),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailoverGroupsEditorState {
    pub server: Option<TargetOverridesResponseDto>,
    pub draft: TargetFailoverOverridesDto,
    pub undo: Vec<TargetFailoverOverridesDto>,
    pub dirty: bool,
    /// Increments with every change of the draft; tags preview requests.
    pub preview_seq: u64,
    /// The newest `preview_seq` whose preview request was answered, successfully or not.
    pub settled_seq: u64,
    pub preview: Option<FailoverGroupListDto>,
    pub selected: Option<GroupRef>,
    /// The server section changed while the draft has unsaved changes, with the etag found on the server.
    pub conflict: Option<Option<String>>,
    /// A save request is in flight.
    pub saving: bool,
    /// The group whose metadata the latest undo step edited; further metadata edits of it extend that step.
    meta_run: Option<GroupRef>,
}

impl FailoverGroupsEditorState {
    pub fn new(target: &str) -> Self {
        Self {
            server: None,
            draft: TargetFailoverOverridesDto::new(target),
            undo: Vec::new(),
            dirty: false,
            preview_seq: 0,
            settled_seq: 0,
            preview: None,
            selected: None,
            conflict: None,
            saving: false,
            meta_run: None,
        }
    }

    /// The shown preview does not belong to the current draft yet.
    pub const fn preview_stale(&self) -> bool { self.settled_seq != self.preview_seq }

    fn server_overrides(&self) -> TargetFailoverOverridesDto {
        self.server
            .as_ref()
            .map_or_else(|| TargetFailoverOverridesDto::new(&self.draft.target), |s| s.overrides.clone())
    }

    fn refresh_dirty(&mut self) {
        self.dirty = match &self.server {
            Some(server) => self.draft != server.overrides,
            None => !self.draft.is_empty(),
        };
    }

    fn load(&mut self, response: TargetOverridesResponseDto) {
        self.draft = response.overrides.clone();
        self.server = Some(response);
        self.undo.clear();
        self.meta_run = None;
        self.dirty = false;
        self.conflict = None;
        self.preview_seq += 1;
    }

    fn edit(&mut self, op: &DraftOp) {
        let before = self.draft.clone();
        let created = op.apply(&mut self.draft);
        if self.draft == before {
            return;
        }
        let meta_group = match op {
            DraftOp::SetMeta { group, .. } => Some(group.clone()),
            _ => None,
        };
        // Typing in the metadata form is one undo step per run of edits of the same group.
        if meta_group.is_none() || meta_group != self.meta_run {
            if self.undo.len() >= MAX_UNDO_STEPS {
                self.undo.remove(0);
            }
            self.undo.push(before);
        }
        self.meta_run = meta_group;
        self.preview_seq += 1;
        if created.is_some() {
            self.selected = created;
        } else if let Some(GroupRef::Manual(id)) = &self.selected {
            if !self.draft.groups.iter().any(|group| group.id == *id) {
                self.selected = None;
            }
        }
        self.refresh_dirty();
    }

    fn apply(&mut self, action: FailoverGroupsEditorAction) {
        match action {
            FailoverGroupsEditorAction::Loaded(response) => self.load(response),
            FailoverGroupsEditorAction::Edit(op) => self.edit(&op),
            FailoverGroupsEditorAction::Undo => {
                if let Some(previous) = self.undo.pop() {
                    self.meta_run = None;
                    self.draft = previous;
                    self.preview_seq += 1;
                    self.refresh_dirty();
                }
            }
            FailoverGroupsEditorAction::PreviewRequested(seq) => self.preview_seq = self.preview_seq.max(seq),
            FailoverGroupsEditorAction::PreviewLoaded(seq, preview) => {
                if seq >= self.preview_seq {
                    self.preview = Some(preview);
                    self.settled_seq = seq;
                }
            }
            FailoverGroupsEditorAction::PreviewFailed(seq) => {
                if seq >= self.preview_seq {
                    self.settled_seq = seq;
                }
            }
            FailoverGroupsEditorAction::SaveStarted => self.saving = true,
            FailoverGroupsEditorAction::SaveFinished => self.saving = false,
            FailoverGroupsEditorAction::Select(selected) => self.selected = selected,
            FailoverGroupsEditorAction::Saved(result, saved) => {
                self.server =
                    Some(TargetOverridesResponseDto { etag: result.etag, file: result.file, overrides: saved });
                self.conflict = None;
                self.meta_run = None;
                self.refresh_dirty();
            }
            FailoverGroupsEditorAction::Conflict(current_etag) => self.conflict = Some(current_etag),
            FailoverGroupsEditorAction::Discard => {
                self.draft = self.server_overrides();
                self.undo.clear();
                self.meta_run = None;
                self.dirty = false;
                self.conflict = None;
                self.preview_seq += 1;
            }
            FailoverGroupsEditorAction::ExternalChange(response) => {
                // The notification of our own write can arrive before the answer of the save.
                if self.saving {
                    return;
                }
                if self.server.as_ref().is_some_and(|server| server.etag == response.etag) {
                    return;
                }
                if self.dirty {
                    self.conflict = Some(response.etag);
                } else {
                    self.load(response);
                }
            }
        }
    }
}

impl Reducible for FailoverGroupsEditorState {
    type Action = FailoverGroupsEditorAction;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        let mut next = (*self).clone();
        next.apply(action);
        Rc::new(next)
    }
}

#[cfg(test)]
#[path = "state.test.rs"]
mod tests;
