use anyhow::Context;
use bstr::BStr;
use but_core::{RefMetadata, extract_remote_name_and_short_name};
use tracing::instrument;

use crate::{
    Workspace,
    workspace::{Segment, SegmentStack, WorkspaceKind},
};

mod queries;
pub use queries::StackTip;
#[cfg(feature = "legacy")]
pub use queries::legacy::HeadStatus;

/// Lifecycle
impl Workspace {
    /// Redo the graph traversal with the same settings as before, but use the latest
    /// data from `repo`, `meta` and `project_meta` to do it.
    /// This is useful to make this instance represent changes to `repo` or `meta`.
    ///
    /// Pass a freshly read `project_meta` to pick up target changes as well, or
    /// `self.ctx.project_meta.clone()` to deliberately keep the current one,
    /// e.g. in the middle of an operation.
    #[instrument(
        name = "Workspace::refresh_from_head",
        level = "debug",
        skip_all,
        err(Debug)
    )]
    pub fn refresh_from_head(
        &mut self,
        repo: &gix::Repository,
        meta: &impl RefMetadata,
        project_meta: but_core::ref_metadata::ProjectMeta,
    ) -> anyhow::Result<()> {
        *self = Workspace::from_head(repo, meta, project_meta, self.ctx.options.clone())?;
        Ok(())
    }

    /// Refresh this instance by projecting `commit_graph` directly — typically the rebase
    /// editor's mutated arena, which IS the next workspace, so no repository rewalk is
    /// needed — mutate-then-project and rewalk-then-project are equivalent.
    ///
    /// Falls back to a rewalk when the commit graph has nothing to project: HEAD is unborn
    /// (e.g. its referent was deleted without a repoint) or points outside the graph.
    #[instrument(
        name = "Workspace::refresh_from_commit_graph",
        level = "debug",
        skip_all,
        err(Debug)
    )]
    pub fn refresh_from_commit_graph(
        &mut self,
        commit_graph: crate::CommitGraph,
        repo: &gix::Repository,
        meta: &impl RefMetadata,
    ) -> anyhow::Result<()> {
        let project_meta = self.ctx.project_meta.clone();
        let options = self.ctx.options.clone();
        let Some(mutated) = crate::workspace_from_commit_graph(
            commit_graph,
            repo,
            meta,
            project_meta.clone(),
            options,
            crate::walk::Overlay::default(),
        )?
        else {
            return self.refresh_from_head(repo, meta, project_meta);
        };
        *self = mutated;
        Ok(())
    }

    /// Adopt the workspace a mutating operation returned. `None` — the operation changed
    /// nothing — leaves this workspace untouched, which is exactly right: it is still current.
    pub fn adopt(&mut self, updated: Option<crate::Workspace>) {
        if let Some(updated) = updated {
            *self = updated;
        }
    }
}

/// Query
impl Workspace {
    /// Return `true` if the workspace has workspace metadata associated with it.
    /// This is relevant when creating references for example.
    pub fn has_metadata(&self) -> bool {
        self.metadata.is_some()
    }

    /// Return the name of the workspace reference by looking our segment up in `graph`.
    /// Note that for managed workspaces, this can be retrieved via [`WorkspaceKind::Managed`].
    pub fn ref_name(&self) -> Option<&gix::refs::FullNameRef> {
        self.tip_ref_info.as_ref().map(|ri| ri.ref_name.as_ref())
    }

    /// Like [`Self::ref_name()`], but owned — for callers that store or move the name.
    pub fn ref_name_owned(&self) -> Option<gix::refs::FullName> {
        self.ref_name().map(ToOwned::to_owned)
    }

    /// Like [`Self::ref_name()`], but return a generic `<anonymous>` name for unnamed workspaces.
    pub fn ref_name_display(&self) -> &BStr {
        self.ref_name()
            .map_or("<anonymous>".into(), |rn| rn.as_bstr())
    }
}

/// Utilities
impl Workspace {
    /// Return the name of the remote most closely associated with this workspace.
    /// In order, we try:
    /// - The remote name of the [Self::target_ref].
    /// - The remote name configured in [workspace metadata](Self::metadata).
    ///
    /// The caller *may* consider falling back to [`gix::Repository::remote_default_name()`],
    /// but beware that one should handle ambiguity if there are more than one remotes.
    pub fn remote_name(&self) -> Option<String> {
        if let Some(tr) = self.target_ref.as_ref() {
            // TODO: should we rather get remote configuration from the repository?
            let remote_names = self
                .ctx
                .symbolic_remote_names
                .iter()
                .map(|name| name.as_str().into())
                .collect();
            extract_remote_name_and_short_name(tr.ref_name.as_ref(), &remote_names)
                .map(|(remote_name, _)| remote_name)
        } else {
            self.ctx.project_meta.push_remote.clone()
        }
    }

    /// Return the resolved target commit ID for use as a base for new branches.
    ///
    /// Prefers the stored [`Self::target_commit`] (the last-synced target SHA),
    /// falling back to the tip of [`Self::target_ref`] (the remote tracking branch).
    /// Does not consider additional traversal seeds.
    ///
    /// Use [`Self::stored_target_commit_id()`] instead when callers need only the explicit
    /// stored target commit without falling back to the target ref tip.
    ///
    /// Returns `None` if neither `target_commit` nor `target_ref` is configured.
    pub fn resolved_target_commit_id(&self) -> Option<gix::ObjectId> {
        self.stored_target_commit_id().or(self.target_ref_commit_id)
    }

    /// Return the `(merge-base, target-commit-id)` of the merge-base between the `commit_to_merge`
    /// and the effective target side (target ref, then stored target commit, then the first
    /// integrated traversal tip).
    /// Return `None` when none of these is set, or if there was no merge-base.
    ///
    /// Use this to get the merge-base for test-merges between `commit_to_merge` and the target,
    /// whose commit is also returned as `target-commit-id`.
    pub fn merge_base_with_target_branch(
        &self,
        commit_to_merge: impl Into<gix::ObjectId>,
    ) -> Option<(gix::ObjectId, gix::ObjectId)> {
        let commit_to_merge = commit_to_merge.into();
        let cg = self.commit_graph();
        cg.node(commit_to_merge)?;
        let target_commit_id = self.effective_target_commit_id()?;
        let merge_base = cg.merge_base(commit_to_merge, target_commit_id)?;
        Some((merge_base, target_commit_id))
    }

    /// Return `true` if the workspace itself is where `HEAD` is pointing to.
    /// If `false`, one of the stack-segments is checked out instead.
    ///
    /// Resolved against the segment graph, not the pruned display: entrypoint-ness is a
    /// structural fact, and pruning can drop the very segment carrying the entrypoint mark.
    pub fn is_entrypoint(&self) -> bool {
        // A frame FACT, not a derivation: the entrypoint marks are gated on exactly this
        // condition (`mark_entrypoint_segments`), verified equivalent across the suites
        // (249 probes, both branches). The tripwire below keeps debug builds honest.

        !(self.frame.entry_inside && self.frame.kind.has_managed_ref())
    }

    /// The `(stack_idx, segment_idx)` of the entry segment in the SEGMENT GRAPH, located from the
    /// FRAME facts — its named segment first, else the segment holding the entry's resolved
    /// commit — the same order [`mark_entrypoint_segments`](super::init) writes the display
    /// marks in.
    pub(crate) fn entry_location(&self) -> Option<(usize, usize)> {
        let (ep_name, ep_commit) = self.frame.entry_facts(&self.ctx);
        self.segment_graph
            .stacks
            .iter()
            .enumerate()
            .find_map(|(stack_idx, stack)| {
                stack
                    .segments
                    .iter()
                    .position(|seg| ep_name.is_some() && seg.ref_name() == ep_name)
                    .map(|idx| (stack_idx, idx))
            })
            .or_else(|| ep_commit.and_then(|id| self.segment_containing(id)))
    }

    /// Whether the entry "marks" the segment-graph stack at `stack_idx`, replicating the display
    /// marker's exact semantics (`mark_entrypoint_segments`): nothing marks
    /// unless the mark gate holds (entry inside a managed workspace); a name match anywhere
    /// restricts marking to name matches; otherwise the entry's resolved commit marks every
    /// stack containing it.
    pub fn entry_marks_stack(&self, stack_idx: usize) -> bool {
        use super::frame::EntryMark;
        let Some(stack) = self.segment_graph.stacks.get(stack_idx) else {
            return false;
        };
        match self.frame.entry_mark(&self.ctx, |name| {
            self.segment_graph.segment_location_by_name(name).is_some()
        }) {
            EntryMark::None => false,
            EntryMark::ByName(name) => stack.segments.iter().any(|s| s.ref_name() == Some(name)),
            EntryMark::ByCommit(id) => stack
                .segments
                .iter()
                .any(|segment| segment.commits.contains(&id)),
        }
    }

    /// Return `true` if the branch with `name` is the workspace target or the targets local tracking branch.
    pub fn is_target_or_its_local_tracking(&self, name: &gix::refs::FullNameRef) -> bool {
        let Some(t) = self.target_ref.as_ref() else {
            return false;
        };

        t.ref_name.as_ref() == name
            || self
                .local_tracking_branch(t.ref_name.as_ref())
                .is_some_and(|local_tracking_ref| local_tracking_ref.as_ref() == name)
    }

    /// Return `true` if `name` is contained in the workspace as segment.
    pub fn refname_is_segment(&self, name: &gix::refs::FullNameRef) -> bool {
        self.find_segment_and_stack_by_refname(name).is_some()
    }

    /// Return `true` if `name` is in the ancestry of the workspace entrypoint, and is IN the workspace as well.
    pub fn is_reachable_from_entrypoint(&self, name: &gix::refs::FullNameRef) -> bool {
        if self.ref_name().filter(|_| self.is_entrypoint()) == Some(name) {
            return true;
        }
        if self.is_entrypoint() {
            self.refname_is_segment(name)
        } else {
            // The entry segment is located from the FRAME facts over the SEGMENT GRAPH; the
            // display-flavored entrypoint marks are not consulted (the tripwire keeps
            // debug builds honest).

            self.entry_location().is_some_and(|(stack_idx, idx)| {
                self.segment_graph
                    .stacks
                    .get(stack_idx)
                    .is_some_and(|stack| {
                        stack
                            .segments
                            .get(idx..)
                            .into_iter()
                            .any(|segments| segments.iter().any(|s| s.ref_name() == Some(name)))
                    })
            })
        }
    }

    /// Try to find `name` in any named [`Segment`] and return it along with the
    /// [`SegmentStack`] containing it. Resolved against the reconciled partition (total,
    /// pre-prune), so it answers for natural stacks and totality-kept branches — the
    /// operation-facing lookup.
    pub fn find_segment_and_stack_by_refname(
        &self,
        name: &gix::refs::FullNameRef,
    ) -> Option<(&SegmentStack, &Segment)> {
        let (stack_idx, seg_idx) = self.segment_graph.segment_location_by_name(name)?;
        let stack = &self.segment_graph.stacks[stack_idx];
        Some((stack, &stack.segments[seg_idx]))
    }

    /// The [`Segment`] whose first-parent extent holds `commit_id`, with its
    /// [`SegmentStack`] — resolved against the reconciled partition, the operation-facing
    /// structure, not the pruned display.
    pub fn find_commit_and_containers(
        &self,
        commit_id: gix::ObjectId,
    ) -> Option<(&SegmentStack, &Segment)> {
        let (stack_idx, seg_idx) = self.segment_containing(commit_id)?;
        let stack = &self.segment_graph.stacks[stack_idx];
        Some((stack, &stack.segments[seg_idx]))
    }

    /// Like [`Self::find_commit_and_containers()`], but errors if `commit_id` isn't in the workspace.
    pub fn try_find_commit_and_containers(
        &self,
        commit_id: gix::ObjectId,
    ) -> anyhow::Result<(&SegmentStack, &Segment)> {
        self.find_commit_and_containers(commit_id)
            .with_context(|| format!("Commit {commit_id} isn't part of the workspace"))
    }

    /// Like [`Self::find_segment_and_stack_by_refname`], but fails with an error.
    pub fn try_find_segment_and_stack_by_refname(
        &self,
        name: &gix::refs::FullNameRef,
    ) -> anyhow::Result<(&SegmentStack, &Segment)> {
        self.find_segment_and_stack_by_refname(name)
            .with_context(|| {
                format!(
                    "Couldn't find any stack that contained the branch named '{}'",
                    name.shorten()
                )
            })
    }
}

/// Debugging
impl Workspace {
    /// Produce a distinct and compressed debug string to show at a glance what the workspace is about.
    pub fn debug_string(&self) -> String {
        let ref_debug_string = |ref_name: &gix::refs::FullNameRef,
                                worktree: Option<&crate::Worktree>| {
            crate::debug::ref_debug_string_inner(ref_name, worktree, self.has_multiple_worktrees)
        };
        let (name, sign) = match &self.kind {
            WorkspaceKind::Managed { ref_info } => (
                ref_debug_string(ref_info.ref_name.as_ref(), ref_info.worktree.as_ref()),
                "🏘️",
            ),
            WorkspaceKind::ManagedMissingWorkspaceCommit { ref_info } => (
                ref_debug_string(ref_info.ref_name.as_ref(), ref_info.worktree.as_ref()),
                "🏘️⚠️",
            ),
            WorkspaceKind::AdHoc => (
                self.tip_ref_info.as_ref().map_or("DETACHED".into(), |ri| {
                    ref_debug_string(ri.ref_name.as_ref(), ri.worktree.as_ref())
                }),
                "⌂",
            ),
        };
        let target = self.target_ref.as_ref().map_or_else(
            || "!".to_string(),
            |t| {
                format!(
                    "{target}{ahead}",
                    target = t.ref_name,
                    ahead = if t.commits_ahead == 0 {
                        "".to_string()
                    } else {
                        format!("⇣{}", t.commits_ahead)
                    }
                )
            },
        );
        format!(
            "{meta}{sign}:{name} <> ✓{target}{bound}",
            meta = if self.metadata.is_some() { "📕" } else { "" },
            bound = self
                .lower_bound
                .map(|base| format!(" on {}", base.to_hex_with_len(7)))
                .unwrap_or_default()
        )
    }
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!("Workspace({})", self.debug_string()))
            .field("kind", &self.kind)
            .field("stacks", &self.display_stacks().unwrap_or_default())
            .field("metadata", &self.metadata)
            .field("target_ref", &self.target_ref)
            .field("target_commit", &self.target_commit)
            .finish()
    }
}
