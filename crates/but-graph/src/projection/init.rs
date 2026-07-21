//! Assemble the [`Workspace`]: the frame decides kind, entrypoint, target and bounds
//! (`frame`), the stacks are collected from the arena and its stored ref layout
//! (`collect`), and this module finishes the picture — name-keyed metadata/worktree
//! sidebands, the entrypoint mark, the target fields, then the pruning passes
//! (archived, integrated, sibling territory). The substrate moves onto the finished
//! workspace: the projection carries the graph it was derived from.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::Context;
use but_core::ref_metadata::{StackId, StackKind::Applied};
use gix::{ObjectId, refs::Category};
use tracing::instrument;

use crate::{
    CommitFlags, CommitGraph, Workspace,
    workspace::{
        GraphContext, Stack, StackCommit, StackCommitFlags, StackSegment, TargetCommit, TargetRef,
        WorkspaceKind, find_segment_owner_indexes_by_refname,
    },
};

/// Reconcile the workspace SUBSTRATE from the commit graph and the build's context: the
/// frame facts, the reconciled [`view`](Workspace::view), and the carried graph —
/// everything the application view needs EXCEPT the display projection. Stop here to hold a
/// fully-constructed workspace without pruning; [`Workspace::project`] is the optional final
/// layer that derives [`stacks`](Workspace::stacks) for the UI.
///
/// Every location of `HEAD` — the entrypoint — reconciles to SOME workspace. Expensive
/// per-commit reads are spent only on commits the user can interact with.
///
/// Target commit ids and integrated traversal seeds can extend the
/// workspace to include these commits to define its lowest base.
#[instrument(name = "reconcile_workspace", level = "trace", skip(cg), err(Debug))]
pub(crate) fn reconcile_workspace(
    mut cg: CommitGraph,
    ctx: GraphContext,
) -> anyhow::Result<crate::workspace::Workspace> {
    let f = super::frame::WorkspaceFrame::derive(&cg, &ctx)
        .context("BUG: the graph must have an entrypoint seed to project a workspace")?;
    // Anchor the integrated foundation STRICTLY BELOW the lower bound (the base's ancestors)
    // so the rebase editor bounds its mutable set to the workspace's own commits. The base
    // itself stays editable — operations may reorder around it, and when the merge-base
    // coincides with a workspace commit (e.g. `main`) the operation must be able to rewrite
    // it. Seeding from the base's parents flags everything below without flagging the base.
    let below_bound: Vec<gix::ObjectId> = f
        .lower_bound
        .and_then(|base| cg.node(base).map(|n| n.parent_ids.clone()))
        .unwrap_or_default();
    cg.set_flag_on_ancestors(crate::CommitFlags::BelowBound, below_bound);
    let commits_below_tip = super::frame::commits_below_tip(&cg, &f);
    let mut target_ref = f.target_ref.as_ref().map(|t| TargetRef {
        ref_name: t.name.clone(),
        tip_commit_id: t.tip,
        commits_ahead: 0,
    });

    // One traversal serves both: the ids feed the workspace, their count the target.
    let incoming_target_commit_ids = target_ref.as_ref().map(|t| {
        t.tip_commit_id
            .map(|tip| super::upstream_commits(&cg, tip, f.lower_bound))
            .unwrap_or_default()
    });
    if let Some(target) = target_ref.as_mut() {
        target.commits_ahead = incoming_target_commit_ids.as_ref().map_or(0, Vec::len);
    }

    // Capture the facts queries need once the segment view is gone, in field order.
    // The entry's commit is the ref's GIT target — a commitless entry segment
    // resolves BELOW its ref, but the entrypoint stays where HEAD pointed.
    let entrypoint = Some(crate::workspace::EntrypointInfo {
        commit_id: f
            .entry_ref
            .as_ref()
            .and_then(|r| cg.ref_target(r))
            .or_else(|| {
                // A commitless entry ref sits on no node — the walk's seed
                // still remembers where it pointed. Edited arenas keep their
                // stale walk seeds; only a live node counts.
                cg.seeds
                    .iter()
                    .find(|s| s.is_entrypoint)
                    .map(|s| s.id)
                    .filter(|id| cg.node(*id).is_some())
            })
            .or(f.entry_commit)
            // Seam-built arenas carry no walk artifacts at all; an ad-hoc
            // view's entry IS its tip.
            .or(f.tip_commit),
        ref_name: f.entry_ref.clone(),
        is_ws_tip: !f.entry_inside,
    });
    let target_ref_commit_id = target_ref.as_ref().and_then(|t| t.tip_commit_id);
    let effective_target_commit_id = target_ref_commit_id
        .or(f.target_commit)
        .or_else(|| f.integrated_tip_commits.first().copied());
    let has_multiple_worktrees = {
        let mut kinds = cg.ref_worktrees().map(|wt| &wt.kind).chain(
            ctx.branch_details
                .values()
                .filter_map(|d| d.worktree.as_ref().map(|wt| &wt.kind)),
        );
        let first = kinds.next();
        first.is_some_and(|first| kinds.any(|kind| kind != first))
    };
    let target_commit = f.target_commit.map(|commit_id| TargetCommit { commit_id });
    let upstream_advanced_past_target = !f.integrated_tip_commits.is_empty();
    // The substrate is complete. The frame is retained VERBATIM as the derivation
    // input — the public fact fields below are copies for callers (legacy mutation
    // included) and never feed the derivation. The reconciliation runs once, EAGERLY,
    // reduced to the segment-graph fact; the display re-derives per call.
    let segment_graph =
        crate::workspace::SegmentGraph::from_stacks(&reconciled_stacks(&cg, &ctx, &f));
    Ok(crate::workspace::Workspace {
        kind: f.kind.clone(),
        lower_bound: f.lower_bound,
        target_ref,
        target_commit,
        metadata: f.metadata.clone(),
        segment_graph,
        commit_graph: cg,
        ctx,
        tip_ref_info: f.tip_ref_info.clone(),
        tip_commit_id: f.tip_commit,
        lower_bound_ref_name: f.lower_bound_ref_name.clone(),
        entrypoint,
        target_ref_commit_id,
        effective_target_commit_id,
        commits_below_tip,
        incoming_target_commit_ids,
        upstream_advanced_past_target,
        has_multiple_worktrees,
        frame: f,
    })
}

impl Workspace {
    /// The pruned DISPLAY stacks, derived per call — a fresh reconciliation run plus the
    /// prune passes: archived, out-of-cone empties, integrated, remote reachability, and
    /// the single-stack base truncation — so display cost lives at the call site: bind it
    /// where used. UI only; operations query the segment graph and never depend on the pruned
    /// shape.
    #[instrument(name = "project", level = "trace", skip(self), err(Debug))]
    pub fn display_stacks(&self) -> anyhow::Result<Vec<Stack>> {
        let mut stacks = materialize_segment_graph(
            &self.segment_graph,
            &self.commit_graph,
            &self.ctx,
            &self.frame,
        );
        let cg = &self.commit_graph;
        let tip_ref_name = self.tip_ref_info.as_ref().map(|ri| ri.ref_name.clone());
        self.prune_archived_segments(&mut stacks);
        self.prune_out_of_cone_empties(&mut stacks, cg);
        {
            // Prune inputs at commit granularity: the advanced-upstream signal, the
            // target's own top commit (the commit its ref really names, if any), and
            // the tip's name for ad-hoc keeps.
            let upstream_advanced_past_target = self.upstream_advanced_past_target;
            let target_top_anchor =
                self.target_commit
                    .as_ref()
                    .map(|tc| tc.commit_id)
                    .or_else(|| {
                        let name = &self.target_ref.as_ref()?.ref_name;
                        let layout = cg.layout()?;
                        let facts = layout.facts_of(name.as_ref())?;
                        (facts.names_segment && !facts.names_empty_segment)
                            .then(|| layout.positioned_on(name.as_ref()))
                            .flatten()
                    });
            self.prune_integrated_segments(
                &mut stacks,
                cg,
                upstream_advanced_past_target,
                target_top_anchor,
                tip_ref_name.as_ref().map(|r| r.as_ref()),
            );
        }
        self.mark_remote_reachability(&mut stacks, cg)?;
        self.add_commits_on_remote(&mut stacks, cg);
        self.truncate_single_stack_to_match_base(&mut stacks);
        debug_assert_applied_stacks_projected(cg, &stacks, self);
        Ok(stacks)
    }
}

/// THE reconciliation core: derive the workspace's stacks from the layout the builder stored
/// on `cg` and the projection frame, plus the layoutless fallback (an unborn ref still names
/// one empty stack). This is the single authority for ref→stack membership, tip-ness, and
/// at-bound surfacing — the facts every operation reads through the reconciled
/// [`view`](Workspace::view). The projection layers display framing (branch details,
/// entrypoint marks) on top; kept a free function of `(graph, context, frame)` so the
/// reconciliation can be exercised on its own.
/// Materialize display stacks FROM the stored segment graph: extents are given (tip→boundary
/// walks), so no claiming runs — only per-commit materialization and name-keyed
/// enrichment. This is the display twin of the eager reconciliation that produced the
/// segment graph; the shared output passes (enrichment, marks) keep the two coherent.
pub(super) fn materialize_segment_graph(
    segment_graph: &crate::workspace::SegmentGraph,
    cg: &CommitGraph,
    ctx: &GraphContext,
    f: &super::frame::WorkspaceFrame,
) -> Vec<Stack> {
    use super::collect;
    let tip_ref_name = f.tip_ref_info.as_ref().map(|ri| ri.ref_name.clone());
    let frame_entry_ref = f
        .entry_inside
        .then(|| f.entry_ref.clone())
        .flatten()
        .or_else(|| tip_ref_name.clone());
    let idx = cg.layout().and_then(|layout| {
        let (ws_commit, _materialized) = collect::resolve_view_anchor(
            cg,
            layout,
            f.metadata.as_ref(),
            frame_entry_ref.as_ref(),
        )?;
        let anchor = collect::ViewAnchor::new(
            cg.is_managed_ws_commit(ws_commit) && f.metadata.is_some(),
            f.kind.has_managed_ref(),
        );
        Some(collect::index_layout(cg, layout, anchor))
    });
    // A detached checkout hands its name back to the commit LAST, after tags.
    let detached = cg.seeds.iter().any(|t| t.is_entrypoint && t.is_detached);
    let mut stacks: Vec<Stack> = segment_graph
        .stacks
        .iter()
        .map(|skel_stack| {
            let segments = skel_stack
                .segments
                .iter()
                .map(|s| {
                    let mut seg = match &s.ref_name {
                        Some(name) => collect::named_segment(name.clone(), s.tip(), ctx),
                        None => StackSegment::default(),
                    };
                    seg.name_projected_from_outside = s.name_projected_from_outside;
                    if s.name_projected_from_outside
                        && let Some(idx) = idx.as_ref()
                        && let Some(tip) = s.tip()
                        && let Some((_, outside)) = idx.projected_at.get(&tip)
                    {
                        seg.commits_outside = Some(outside.clone());
                    }
                    let mut last_walked = None;
                    for &id in &s.commits {
                        let Some(node) = cg.node(id) else { break };
                        let mut commit = StackCommit::from_graph_commit(node);
                        if let Some(idx) = idx.as_ref() {
                            collect::strip_structural_refs(
                                &mut commit,
                                seg.ref_name(),
                                &idx.names_empty,
                            );
                            if detached
                                && let Some(&name) = idx.naming_at.get(&id)
                                && let Some(i) = commit
                                    .refs
                                    .iter()
                                    .position(|ri| ri.ref_name.as_ref() == name.as_ref())
                            {
                                let ri = commit.refs.remove(i);
                                commit.refs.push(ri);
                            }
                        }
                        seg.commits.push(commit);
                        last_walked = Some(id);
                    }
                    // A commit whose raw parents were never walked ends the extent early —
                    // the traversal's limit, worn by the last collected commit.
                    if let Some(last) = last_walked
                        && cg.first_parent(last).is_none()
                        && let Some(node) = cg.node(last)
                        && !node.parent_ids.is_empty()
                        && !node.flags.contains(crate::CommitFlags::ShallowBoundary)
                        && let Some(commit) = seg.commits.last_mut()
                    {
                        commit.flags |= crate::workspace::StackCommitFlags::EarlyEnd;
                    }
                    seg.base = s.base;
                    seg
                })
                .collect();
            Stack::from_base_and_segments_raw(segments, skel_stack.id)
        })
        .collect();
    enrich_from_branch_details(&mut stacks, ctx);
    mark_entrypoint_segments(&mut stacks, f, ctx);
    stacks
}

fn reconcile_membership(
    cg: &CommitGraph,
    ctx: &GraphContext,
    f: &super::frame::WorkspaceFrame,
) -> Vec<Stack> {
    let tip_ref_name = f.tip_ref_info.as_ref().map(|ri| ri.ref_name.clone());
    let frame_entry_ref = f
        .entry_inside
        .then(|| f.entry_ref.clone())
        .flatten()
        .or_else(|| tip_ref_name.clone());
    let frame_entry_commit = cg.entrypoint();
    super::collect::stacks_from_arena(
        cg,
        ctx,
        f.lower_bound,
        f.metadata.as_ref(),
        f.kind.has_managed_ref(),
        frame_entry_ref.as_ref(),
        frame_entry_commit,
    )
    .unwrap_or_else(|| {
        // Layoutless graphs — unborn refs — still name one empty stack.
        tip_ref_name
            .map(|name| {
                vec![Stack::from_base_and_segments_raw(
                    vec![super::collect::named_segment(name, None, ctx)],
                    Some(StackId::single_branch_id()),
                )]
            })
            .unwrap_or_default()
    })
}

/// The reconciled stacks BOTH the display view and the mutating operations consume: the
/// [`reconcile_membership`] core plus the two marks operations read — per-branch metadata
/// ([`enrich_from_branch_details`]) and the entrypoint marks ([`mark_entrypoint_segments`]).
/// Worktree info rides along in the enrichment but is display-only (no operation reads it);
/// pruning is a separate layer ([`Workspace::project`]). A free function of
/// `(graph, context, frame)` so the reconciliation can be exercised, and reused, on its own.
/// The reconciliation as the SEGMENT GRAPH reduces it: membership plus name-keyed enrichment.
/// Entrypoint marks are display decoration — [`materialize_segment_graph`] applies them; the
/// segment graph stores none, so the eager reduction never computes them.
pub(super) fn reconciled_stacks(
    cg: &CommitGraph,
    ctx: &GraphContext,
    f: &super::frame::WorkspaceFrame,
) -> Vec<Stack> {
    let mut stacks = reconcile_membership(cg, ctx, f);
    enrich_from_branch_details(&mut stacks, ctx);
    stacks
}

/// Name-keyed enrichment from the builder's capture: metadata sidebands and worktree
/// info ride on names, not structure.
fn enrich_from_branch_details(stacks: &mut [Stack], ctx: &GraphContext) {
    for seg in stacks.iter_mut().flat_map(|s| &mut s.segments) {
        let Some(details) = seg.ref_name().and_then(|name| ctx.branch_details.get(name)) else {
            continue;
        };
        seg.metadata = details.metadata.clone();
        if let Some(ri) = seg.ref_info.as_mut() {
            ri.worktree = details.worktree.clone();
        }
    }
}

/// The entrypoint mark mirrors the frame's entrypoint: its named segment, else the
/// first segment holding the entrypoint commit. Only managed workspaces mark — ad-hoc
/// views ARE their entrypoint.
fn mark_entrypoint_segments(
    stacks: &mut [Stack],
    f: &super::frame::WorkspaceFrame,
    ctx: &GraphContext,
) {
    use super::frame::EntryMark;
    let mark = f.entry_mark(ctx, |name| {
        stacks
            .iter()
            .flat_map(|s| &s.segments)
            .any(|seg| seg.ref_name() == Some(name))
    });
    match mark {
        EntryMark::None => {}
        EntryMark::ByName(name) => {
            for seg in stacks
                .iter_mut()
                .flat_map(|s| &mut s.segments)
                .filter(|seg| seg.ref_name() == Some(name))
            {
                seg.is_entrypoint = true;
            }
        }
        EntryMark::ByCommit(id) => {
            for seg in stacks.iter_mut().flat_map(|s| &mut s.segments) {
                if seg.commits.iter().any(|c| c.id == id) {
                    seg.is_entrypoint = true;
                }
            }
        }
    }
}

/// Test-build tripwire: an APPLIED metadata stack silently disappearing from `stacks`.
/// The persistence danger is gone (the reconcile removal: no write reads the view back) and
/// the merge resolves named tips from metadata + the graph. Operations no longer read this
/// projection for structure either — they resolve on the reconciled `view` (a raw-graph
/// resolution was proven impossible: tip-ness is irreducibly reconciled). So this is now a
/// DISPLAY-COMPLETENESS guard — applied branches must stay visible to the UI — that doubles
/// as a forward-guard against the projection→metadata anti-pattern returning. The
/// trace/keep-list machinery it guards is genuine display machinery, not operation
/// scaffolding: relaxing it would regress the UI, not operations. Scoped to managed
/// workspaces; a stack counts only when at least one non-archived branch of it actually
/// names a segment in this graph.
#[cfg(debug_assertions)]
fn debug_assert_applied_stacks_projected(cg: &CommitGraph, stacks: &[Stack], ws: &Workspace) {
    if !matches!(ws.kind, WorkspaceKind::Managed { .. }) {
        return;
    }
    let Some(meta) = ws.metadata.as_ref() else {
        return;
    };
    let projected: std::collections::HashSet<_> = stacks
        .iter()
        .flat_map(|s| s.segments.iter())
        .filter_map(|s| s.ref_name().map(|r| r.to_owned()))
        .collect();
    // Only branches REACHABLE from the workspace tip count: metadata is the DESIRED state
    // and legitimately runs ahead of the graph mid-operation (apply writes metadata before
    // the workspace merge is rebuilt). The failure class is a branch that IS in the
    // workspace's reach and still lost its stack. Reachability is seeded from the tip and
    // everything below it — the below-walk stops at the bound, the parent walk continues.
    let mut reachable = std::collections::HashSet::new();
    let mut queue: Vec<_> = ws
        .tip_commit_id
        .into_iter()
        .chain(ws.commits_below_tip.iter().copied())
        .collect();
    while let Some(id) = queue.pop() {
        if !reachable.insert(id) {
            continue;
        }
        queue.extend(cg.all_parent_ids(id));
    }
    /// How the workspace incorporates a branch's resting commit. Classified
    /// INTEGRATED before in-workspace: target-absorbed work is integrated whether or
    /// not the workspace still spans it (an unpulled target tip is Integrated).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Territory {
        InWorkspace,
        Integrated,
        Outside,
        Unborn,
    }
    let layout = cg.layout();
    for stack in meta.stacks(but_core::ref_metadata::StackKind::Applied) {
        for branch in stack.branches.iter().filter(|branch| !branch.archived) {
            let name = branch.ref_name.as_ref();
            let facts = layout.and_then(|layout| layout.facts_of(name));
            let rest = layout.and_then(|layout| layout.positioned_on(name));
            let territory = match rest {
                None => Territory::Unborn,
                Some(on) => match cg.node(on) {
                    None => Territory::Outside,
                    Some(node) if node.flags.contains(crate::CommitFlags::Integrated) => {
                        Territory::Integrated
                    }
                    Some(_) if reachable.contains(&on) => Territory::InWorkspace,
                    Some(_) => Territory::Outside,
                },
            };
            // Only a branch that NAMES a segment can be demanded of the view (a passive
            // rider is representable only through its carrier). Live workspace content
            // must always show; INTEGRATED territory away from the lower bound is
            // deliberately not overzealously materialized — an empty or a rest ON the
            // bound keeps its stack.
            let represented = facts.is_some_and(|facts| facts.names_segment)
                && match territory {
                    Territory::InWorkspace => true,
                    Territory::Integrated => {
                        facts.is_some_and(|facts| facts.names_empty_segment)
                            || rest == ws.lower_bound
                    }
                    Territory::Outside | Territory::Unborn => false,
                };
            debug_assert!(
                !represented || projected.contains(name),
                "applied branch {:?} of stack {:?} ({territory:?}) vanished from the projection — \
                 operations writing from this projection would drop it on disk",
                name.as_bstr(),
                stack.id,
            );
        }
    }
}

#[cfg(not(debug_assertions))]
fn debug_assert_applied_stacks_projected(_cg: &CommitGraph, _stacks: &[Stack], _ws: &Workspace) {}

impl Workspace {
    /// Prune segments whose branch is marked archived in workspace metadata — matched by name,
    /// that's all we have — top to bottom, and only when everything below them is also empty:
    /// a non-empty tail keeps the segment visible regardless of the flag. A stack whose every
    /// segment was archived is removed entirely. The flag deliberately stays out of the
    /// segment data itself so archived handling remains local to this pass.
    /// Empty segments resting on target-only history — integrated commits the workspace
    /// tip cannot reach — are not displayed: such a branch (typically integrated while
    /// upstream advanced past the stored target) has left the workspace cone. The
    /// reconciliation keeps it for totality, so operations still see and un-apply it.
    fn prune_out_of_cone_empties(&self, stacks: &mut Vec<Stack>, cg: &crate::CommitGraph) {
        if self.target_ref.is_none() {
            return;
        }
        for stack in stacks.iter_mut() {
            stack.segments.retain(|segment| {
                if !segment.commits.is_empty() {
                    return true;
                }
                let Some(anchor) = segment.base else {
                    return true;
                };
                let Some(node) = cg.node(anchor) else {
                    return true;
                };
                !node.flags.contains(crate::CommitFlags::Integrated)
                    || node.flags.contains(crate::CommitFlags::InWorkspace)
            });
        }
        stacks.retain(|stack| !stack.segments.is_empty());
    }

    fn prune_archived_segments(&self, stacks: &mut Vec<Stack>) {
        // Explicit substrate path: keeps the borrow disjoint from `stacks` below.
        let Some(md) = &self.metadata else {
            return;
        };
        let archived_stack_branches = md.stacks(Applied).flat_map(|s| {
            s.branches
                .iter()
                .filter_map(|s| s.archived.then_some(s.ref_name.as_ref()))
        });
        let mut empty_stacks_to_remove = Vec::new();
        for archived_ref_name in archived_stack_branches {
            let Some((stack_idx, segment_idx)) =
                find_segment_owner_indexes_by_refname(stacks, archived_ref_name)
            else {
                continue;
            };
            let stack = &mut stacks[stack_idx];
            let all_downwards_are_empty = stack.segments[segment_idx..]
                .iter()
                .all(|s| s.commits.is_empty());
            if !all_downwards_are_empty {
                continue;
            }
            stack.segments.truncate(segment_idx);
            if stack.segments.is_empty() {
                empty_stacks_to_remove.push(stack_idx);
            }
        }

        empty_stacks_to_remove.sort();
        for stack_idx_to_remove in empty_stacks_to_remove.into_iter().rev() {
            let stack = stacks.remove(stack_idx_to_remove);
            tracing::warn!(
                "Pruned stack {stack_id:?} from workspace as all its segments were archived",
                stack_id = stack.id
            )
        }
    }

    /// Remove integrated commits and empty branches at the bottom of each
    /// stack, but only those at or below the workspace's target commit.
    /// Integrated commits above the target commit are kept until the user advances
    /// the target via upstream integration.
    // TODO: the per-stack fork point is recomputed on every projection rather than
    //       stored; persisting it would avoid re-deriving the target trunk each build.
    fn prune_integrated_segments(
        &self,
        stacks: &mut Vec<Stack>,
        cg: &crate::CommitGraph,
        upstream_advanced_past_target: bool,
        target_top_anchor: Option<ObjectId>,
        ws_tip_ref: Option<&gix::refs::FullNameRef>,
    ) {
        // Integrated-commit pruning only applies to workspaces tracking an upstream
        // target ref; without one, leave the stacks untouched.
        if self.target_ref.is_none() {
            return;
        }
        // Extra integrated tips mean upstream advanced past the stored target. Bail only
        // if there's no stored target *commit* to bound pruning against.
        if self.target_commit.is_none() && upstream_advanced_past_target {
            return;
        }

        // The territory anchors on the target's top commit; an EMPTY target segment
        // anchors on the stored target commit or the target ref's position.
        let target_top = target_top_anchor
            .or_else(|| self.target_commit.as_ref().map(|tc| tc.commit_id))
            .or_else(|| {
                let name = &self.target_ref.as_ref()?.ref_name;
                cg.layout()?.positioned_on(name.as_ref())
            });
        let Some(target_top) = target_top else {
            return;
        };
        let prune_commits = prunable_territory(cg, target_top, upstream_advanced_past_target);
        // Explicit substrate paths from here on: these borrows live across the
        // `&mut stacks` loops below and must stay disjoint from them.
        let metadata = self.metadata.as_ref();
        let keep_empty_names = if matches!(self.kind, WorkspaceKind::AdHoc) {
            self.ctx
                .ad_hoc_branch_stack_orders
                .iter()
                .flatten()
                .map(|ref_name| ref_name.as_ref())
                .chain(ws_tip_ref)
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let keep_if_fully_integrated =
            upstream_advanced_past_target && !matches!(self.kind, WorkspaceKind::AdHoc);
        // The target's LOCAL tracking branch is exempt from integrated pruning when METADATA
        // applies it as a stack in a MANAGED workspace: caught up with the target, ALL its
        // commits are integrated by definition, so pruning would empty the stack by construction
        // and slide its base to the workspace lower bound (`ref_target=M2, base=M1` — an
        // incoherent segment). An applied main keeps its commits: its stack IS the base
        // indicator. The single-branch view of a checked-out main keeps pruning its integrated
        // base as before — membership, and thus the exemption, is metadata-explicit.
        let target_local = self
            .kind
            .has_managed_ref()
            .then(|| {
                self.target_ref
                    .as_ref()
                    .and_then(|tr| {
                        self.ctx
                            .remote_tracking
                            .iter()
                            .find_map(|(local, r)| (r == &tr.ref_name).then_some(local))
                    })
                    .map(|local| local.as_ref())
            })
            .flatten();
        // A below-cut segment survives as a shell when its ref is ANY applied metadata
        // branch — a chain member can be absorbed into a sibling stack's walk, so the host
        // stack's own list is not the authority. Archived branches are excluded, which
        // compensates for `prune_archived_segments` running before integrated pruning:
        // archived segments that still had commits are skipped there, then emptied here.
        let applied_branch_names: std::collections::BTreeSet<&gix::refs::FullNameRef> = metadata
            .map(|meta| {
                meta.stacks(Applied)
                    .flat_map(|ms| {
                        ms.branches
                            .iter()
                            .filter(|b| !b.archived)
                            .map(|b| b.ref_name.as_ref())
                            // Unreconciled legacy metadata can list the workspace ref
                            // itself as a branch; gitbutler/* never rests as a segment.
                            .filter(|rn| !super::collect::in_gitbutler_namespace(rn))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for stack in stacks.iter_mut() {
            let is_target_local_stack =
                stack.id.is_some() && stack.ref_name().is_some_and(|rn| Some(rn) == target_local);
            // Upstream advanced: floor the stack at its fork point but keep a fully-integrated
            // tip in managed workspaces so it survives for `integrate_upstream`. Single-branch
            // mode keeps the branch shell, but prunes integrated target/base commits from it.
            if !is_target_local_stack {
                prune_integrated_stack_segments(
                    stack,
                    &prune_commits,
                    keep_if_fully_integrated,
                    &applied_branch_names,
                );
            }
        }
        let orphan_chain_names = orphan_chain_names(metadata, stacks);
        for stack in stacks.iter_mut() {
            remove_empty_branches(
                stack,
                &applied_branch_names,
                &keep_empty_names,
                &orphan_chain_names,
                matches!(self.kind, WorkspaceKind::AdHoc),
            );
            // Pruning moved the stack's bottom; refresh its base to its own fork point with the
            // target rather than leaving the pre-prune global merge base. Every branch has its
            // own base.
            stack.recompute_last_segment_base_from(cg);
        }
        stacks.retain(|stack| !stack.segments.is_empty());
        hide_bases_on_sibling_territory(stacks);
    }
    /// Trace each linked remote down the ARENA and set commit flags to indicate
    /// whether a commit in the workspace is reachable from a remote, and how.
    fn mark_remote_reachability(
        &self,
        stacks: &mut [Stack],
        cg: &crate::CommitGraph,
    ) -> anyhow::Result<()> {
        // The builder records whether it actually LINKED a remote — an ambiguous
        // remote stays deliberately unlinked, so a name lookup won't do.
        let remote_refs: Vec<_> = stacks
            .iter()
            .flat_map(|s| {
                s.segments.iter().filter_map(|s| {
                    let name = s.ref_name()?;
                    let tip = self.ctx.branch_details.get(name)?.remote_walk_tip?;
                    s.remote_tracking_ref_name.clone().map(|rn| (rn, tip))
                })
            })
            .collect();
        let remote_named_at = remote_naming_positions(cg);
        struct Flagging {
            at: ObjectId,
            remote: gix::refs::FullName,
        }
        let mut flaggings = Vec::new();
        for (remote_tracking_ref_name, tip) in remote_refs {
            let mut seen = HashSet::new();
            let mut queue = vec![tip];
            while let Some(id) = queue.pop() {
                if !seen.insert(id) {
                    continue;
                }
                let Some(node) = cg.node(id) else { continue };
                // Stop at non-remote commits, and never 'steal' commits from other
                // known remote territory.
                let prune = !node.flags.is_remote()
                    || (id != tip
                        && remote_named_at
                            .get(&id)
                            .is_some_and(|&n| n != &remote_tracking_ref_name));
                if prune {
                    if !node.flags.is_remote() {
                        flaggings.push(Flagging {
                            at: id,
                            remote: remote_tracking_ref_name.clone(),
                        });
                    }
                    continue;
                }
                queue.extend(cg.all_parent_ids(id));
            }
        }
        for Flagging { at, remote } in flaggings {
            for stack in stacks.iter_mut() {
                // The remote's reach into this stack starts where the commit appears.
                let Some((first_segment, first_commit_index)) =
                    stack.segments.iter().enumerate().find_map(|(os_idx, os)| {
                        os.commits
                            .iter()
                            .position(|c| c.id == at)
                            .map(|ci| (os_idx, ci))
                    })
                else {
                    continue;
                };
                let mut first_commit_index = Some(first_commit_index);
                for segment in &mut stack.segments[first_segment..] {
                    let remote_reachable_flags =
                        if segment.remote_tracking_ref_name.as_ref() == Some(&remote) {
                            StackCommitFlags::ReachableByMatchingRemote
                        } else {
                            StackCommitFlags::empty()
                        } | StackCommitFlags::ReachableByRemote;
                    for commit in
                        &mut segment.commits[first_commit_index.take().unwrap_or_default()..]
                    {
                        commit.flags |= remote_reachable_flags;
                    }
                }
                // keep looking - other stacks can repeat the commit!
            }
        }
        Ok(())
    }
    /// For each local segment that has a linked remote tracking branch, walk the
    /// remote side of the ARENA and collect commits that exist on the remote but
    /// not locally:
    /// - commits that are purely remote (never existed locally or pre-rebase versions), and
    /// - non-integrated commits from upper stack segments that are still on the
    ///   remote (the "branch split" case — a previously combined push left the
    ///   remote pointing at commits that now belong to branch above it).
    fn add_commits_on_remote(&self, stacks: &mut [Stack], cg: &crate::CommitGraph) {
        let remote_named_at = remote_naming_positions(cg);
        let naming = naming_positions(cg);
        for stack in stacks.iter_mut() {
            let mut above_commit_ids = HashSet::new();
            for seg_idx in 0..stack.segments.len() {
                let Some(tip) = stack.segments[seg_idx]
                    .ref_name()
                    .and_then(|name| self.ctx.branch_details.get(name))
                    .and_then(|d| d.remote_walk_tip)
                else {
                    // Still accumulate this segment's commits for lower segments.
                    above_commit_ids.extend(stack.segments[seg_idx].commits.iter().map(|c| c.id));
                    continue;
                };

                // Run-wise BFS: collect purely-remote commits, stopping runs at
                // local commits or territory another remote-naming position owns.
                let mut remote_commits = Vec::new();
                let mut seen_ids = HashSet::new();
                let mut runs = std::collections::VecDeque::from([tip]);
                while let Some(run_start) = runs.pop_front() {
                    let mut cursor = Some(run_start);
                    while let Some(id) = cursor.take() {
                        if !seen_ids.insert(id) {
                            break;
                        }
                        let Some(node) = cg.node(id) else { break };
                        if !node.flags.is_remote()
                            || (id != tip
                                && remote_named_at.get(&id).is_some_and(|&n| {
                                    stack.segments[seg_idx].remote_tracking_ref_name.as_ref()
                                        != Some(n)
                                }))
                        {
                            break;
                        }
                        let mut commit = StackCommit::from_graph_commit(node);
                        // The territory's naming ref is structure, not a rider —
                        // deduced remotes included.
                        commit.refs.retain(|ri| {
                            !naming.contains(&(commit.id, ri.ref_name.as_ref()))
                                && stack.segments[seg_idx].remote_tracking_ref_name.as_ref()
                                    != Some(&ri.ref_name)
                        });
                        remote_commits.push(commit);
                        let parents = cg.all_parent_ids(id);
                        cursor = parents.first().copied();
                        runs.extend(parents.into_iter().skip(1));
                    }
                }

                // First-parent walk: detect non-integrated commits from upper
                // stack segments that are still reachable by the remote tracking branch.
                if !above_commit_ids.is_empty() {
                    let mut seen: HashSet<_> = remote_commits.iter().map(|c| c.id).collect();
                    let mut extra = Vec::new();
                    let mut cursor = Some(tip);
                    let mut fp_seen = HashSet::new();
                    while let Some(id) = cursor.take() {
                        if !fp_seen.insert(id) {
                            break;
                        }
                        if id != tip && remote_named_at.contains_key(&id) {
                            break;
                        }
                        let Some(node) = cg.node(id) else { break };
                        if above_commit_ids.contains(&id)
                            && !node.flags.contains(CommitFlags::Integrated)
                            && seen.insert(id)
                        {
                            let mut commit = StackCommit::from_graph_commit(node);
                            commit.refs.retain(|ri| {
                                !naming.contains(&(commit.id, ri.ref_name.as_ref()))
                                    && stack.segments[seg_idx].remote_tracking_ref_name.as_ref()
                                        != Some(&ri.ref_name)
                            });
                            extra.push(commit);
                        }
                        cursor = cg.all_parent_ids(id).first().copied();
                    }
                    remote_commits.extend(extra);
                }

                stack.segments[seg_idx].commits_on_remote = remote_commits;

                // Accumulate this segment's commits for lower segments.
                above_commit_ids.extend(stack.segments[seg_idx].commits.iter().map(|c| c.id));
            }
        }
    }

    /// If there is a single stack and the base happens to be itself (which happens if the stack is directly integrated/inline with the target),
    /// then empty all commits and segment-related metadata.
    fn truncate_single_stack_to_match_base(&self, stacks: &mut [Stack]) {
        if stacks.len() != 1 {
            return;
        }
        let Some(stack) = stacks.first_mut() else {
            return;
        };
        let stack_is_base =
            stack
                .segments
                .first()
                .zip(self.lower_bound)
                .is_some_and(|(segment, base)|
                // The stack IS the base when its first commit is the bound commit itself.
                segment.commits.first().is_some_and(|c| c.id == base));
        if !stack_is_base {
            return;
        }

        stack.segments.drain(1..);
        let first_segment = stack.segments.first_mut().expect("non-empty");
        first_segment.commits.clear();
    }
}

/// Prune the integrated tail whose commits are in `prune_commits`; commits in other
/// territory are kept (e.g. above the target, or reaching it only via a merge's 2nd parent).
/// Cutting happens at whole-GROUP boundaries (the prune map's tokens): a group holding any
/// live commit survives in full.
///
/// With `keep_if_fully_integrated`, a stack whose every commit would be pruned is left
/// untouched, keeping a fully-integrated branch's tip visible for `integrate_upstream`.
fn prune_integrated_stack_segments(
    stack: &mut Stack,
    prune_commits: &HashMap<ObjectId, usize>,
    keep_if_fully_integrated: bool,
    metadata_branch_names: &std::collections::BTreeSet<&gix::refs::FullNameRef>,
) {
    // Walk stack segments bottom-up, then prune-groups bottom-up within each stack
    // segment. Stop at the first group that is either not fully integrated or not
    // in the prunable territory.
    let mut cut: Option<(usize, usize)> = None;
    // Whether any commit would survive the cut (not integrated, or not prunable).
    let mut has_surviving_commit = false;
    'outer: for seg_idx in (0..stack.segments.len()).rev() {
        let seg = &stack.segments[seg_idx];
        if seg.commits.is_empty() {
            continue;
        }
        // Contiguous runs of the same prune-group, bottom-up.
        let mut end = seg.commits.len();
        while end > 0 {
            let group = prune_commits.get(&seg.commits[end - 1].id).copied();
            let mut start = end - 1;
            while start > 0 && prune_commits.get(&seg.commits[start - 1].id).copied() == group {
                start -= 1;
            }
            let commits = &seg.commits[start..end];
            if group.is_some() && commits_are_integrated(commits) {
                cut = Some((seg_idx, start));
            } else {
                has_surviving_commit = true;
                break 'outer;
            }
            end = start;
        }
    }

    let Some((cut_seg_idx, cut_offset)) = cut else {
        return;
    };

    // The whole stack is integrated trunk. While upstream is ahead, keep it (e.g. a
    // fully-integrated branch behind its remote) rather than pruning it out of existence.
    if keep_if_fully_integrated && !has_surviving_commit {
        return;
    }

    stack.segments[cut_seg_idx].commits.truncate(cut_offset);

    // Remove all stack segments below the cut. If the cut emptied the topmost
    // stack segment, keep it so `remove_empty_branches` can decide whether its
    // branch ref should be preserved, e.g. a metadata-tracked branch at the fork point.
    let keep = if stack.segments[cut_seg_idx].commits.is_empty() && cut_seg_idx > 0 {
        cut_seg_idx
    } else {
        cut_seg_idx + 1
    };
    // Below the cut, metadata-tracked branches survive as empty shells — their commits are
    // integrated trunk, but the branches themselves are applied workspace state whose fate
    // belongs to `remove_empty_branches` (dropping them here would silently unapply them
    // on the next metadata write). Everything else — trunk naming refs like the target's
    // local — goes with its territory.
    let shells: Vec<StackSegment> = stack
        .segments
        .drain(keep..)
        .filter(|seg| {
            seg.ref_name()
                .is_some_and(|rn| metadata_branch_names.contains(rn))
        })
        .map(|mut seg| {
            seg.commits.clear();
            seg
        })
        .collect();
    stack.segments.extend(shells);
}

/// Remote-category refs that NAME territory, per commit: a commit reachable
/// from two remotes has exactly one deliberate owner.
fn remote_naming_positions(cg: &crate::CommitGraph) -> HashMap<ObjectId, &gix::refs::FullName> {
    cg.layout()
        .map(|l| {
            l.segment_naming_placements()
                .filter(|(name, _)| name.category() == Some(Category::RemoteBranch))
                .map(|(name, on)| (on, name))
                .collect()
        })
        .unwrap_or_default()
}

/// Every (commit, name) pair where the name is a positioned SEGMENT-naming ref of
/// any category — such refs are structure, never riders.
fn naming_positions(cg: &crate::CommitGraph) -> HashSet<(ObjectId, &gix::refs::FullNameRef)> {
    cg.layout()
        .map(|l| {
            l.segment_naming_placements()
                .map(|(name, on)| (on, name.as_ref()))
                .collect()
        })
        .unwrap_or_default()
}

fn commits_are_integrated(commits: &[StackCommit]) -> bool {
    commits
        .iter()
        .all(|commit| commit.flags.contains(StackCommitFlags::Integrated))
}

/// The prunable territory keyed by COMMIT, each with an opaque group token so pruning
/// still cuts at whole-group boundaries: groups break at positioned naming refs, the
/// arena's segmentation marks.
///
/// With upstream advanced past the stored target, only the target's FIRST-PARENT trunk
/// is prunable — commits reaching the target via a merge's second parent are off it, so
/// a branch's own work is preserved. Otherwise the territory is the target's top commit
/// plus all its ancestors.
fn prunable_territory(
    cg: &crate::CommitGraph,
    target_top: ObjectId,
    upstream_advanced_past_target: bool,
) -> HashMap<ObjectId, usize> {
    let naming_at: HashSet<ObjectId> = naming_positions(cg).into_iter().map(|(id, _)| id).collect();
    let mut prune_commits = HashMap::<ObjectId, usize>::new();
    let mut groups = HashMap::<ObjectId, usize>::new();
    let group_of = |anchor: ObjectId, groups: &mut HashMap<ObjectId, usize>| {
        let next = groups.len();
        *groups.entry(anchor).or_insert(next)
    };
    if upstream_advanced_past_target {
        let mut anchor = target_top;
        let mut cursor = Some(target_top);
        while let Some(id) = cursor.take() {
            if naming_at.contains(&id) {
                anchor = id;
            }
            if prune_commits
                .insert(id, group_of(anchor, &mut groups))
                .is_some()
            {
                break;
            }
            cursor = cg.all_parent_ids(id).first().copied();
        }
    } else {
        let mut queue = vec![(target_top, target_top)];
        while let Some((id, mut anchor)) = queue.pop() {
            if naming_at.contains(&id) {
                anchor = id;
            }
            if prune_commits
                .insert(id, group_of(anchor, &mut groups))
                .is_some()
            {
                continue;
            }
            queue.extend(cg.all_parent_ids(id).into_iter().map(|p| (p, anchor)));
        }
    }
    prune_commits
}

/// Branch names of applied chains with no commit-bearing segment in ANY stack. Their
/// empty segments are the chain's last projected trace and must survive removal —
/// operations rebuild metadata from the projection, so a vanished chain gets un-applied
/// on disk (e.g. an applied branch merged upstream, spliced as an empty segment into
/// the stack that walks its commit).
fn orphan_chain_names<'m>(
    metadata: Option<&'m but_core::ref_metadata::Workspace>,
    stacks: &[Stack],
) -> HashSet<&'m gix::refs::FullNameRef> {
    metadata
        .map(|meta| {
            meta.stacks(Applied)
                .filter(|ms| {
                    !stacks.iter().any(|stack| {
                        stack
                            .segments
                            .iter()
                            .filter(|seg| !seg.commits.is_empty())
                            .filter_map(|seg| seg.ref_name())
                            .any(|name| {
                                ms.branches
                                    .iter()
                                    .any(|b| b.ref_name.as_ref() == name && !b.archived)
                            })
                    })
                })
                .flat_map(|ms| {
                    ms.branches
                        .iter()
                        .filter(|b| !b.archived)
                        .map(|b| b.ref_name.as_ref())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A recomputed base is shown only when it lies OUTSIDE the workspace's own stacks; a base
/// resting on a sibling stack's territory is hidden.
fn hide_bases_on_sibling_territory(stacks: &mut [Stack]) {
    let owned: HashSet<ObjectId> = stacks
        .iter()
        .flat_map(|s| s.segments.iter().flat_map(|seg| &seg.commits))
        .map(|c| c.id)
        .collect();
    for stack in stacks {
        let own: HashSet<ObjectId> = stack
            .segments
            .iter()
            .flat_map(|seg| &seg.commits)
            .map(|c| c.id)
            .collect();
        if let Some(seg) = stack.segments.last_mut()
            && seg
                .base
                .is_some_and(|b| owned.contains(&b) && !own.contains(&b))
        {
            seg.base = None;
        }
    }
}

/// Remove empty segments unless they are mentioned in workspace metadata
/// (e.g. a branch the user just added at the fork point with no commits yet).
/// `orphan_chain_names` are branches of applied chains with no commit-bearing
/// segment in ANY stack — their empty segments are the chain's last projected
/// trace and always survive.
fn remove_empty_branches(
    stack: &mut Stack,
    applied_branch_names: &BTreeSet<&gix::refs::FullNameRef>,
    keep_empty_names: &BTreeSet<&gix::refs::FullNameRef>,
    orphan_chain_names: &HashSet<&gix::refs::FullNameRef>,
    keep_tip: bool,
) {
    let bottom_base = stack.segments.last().and_then(|seg| seg.base);
    let mut idx = 0;
    stack.segments.retain(|seg| {
        let is_tip = idx == 0;
        idx += 1;
        (keep_tip && is_tip)
            || !seg.commits.is_empty()
            // Any applied (non-archived) metadata branch keeps its empty segment,
            // WHEREVER it surfaced — a chain member absorbed into a sibling stack's walk
            // is still applied state, and dropping it would unapply it on the next
            // metadata write.
            || seg.ref_name().is_some_and(|rn| {
                applied_branch_names.contains(rn)
                    || keep_empty_names.contains(rn)
                    || orphan_chain_names.contains(rn)
            })
    });
    // Removing an empty bottom doesn't change what the stack rests on: carry the old
    // bottom's base when the new bottom has none — an all-empty stack has no commits for
    // `recompute_last_segment_base_from` to derive it from.
    if let Some(seg) = stack.segments.last_mut()
        && seg.base.is_none()
    {
        seg.base = bottom_base;
    }
}
