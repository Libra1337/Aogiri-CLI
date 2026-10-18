//! The plan follows the main agent's subagents where their state proves a
//! step's progress: the agents the model attached to a step (`plan_step` on
//! spawn_agent, or the update_plan step whose `agent` names one). A step to
//! do turns in_progress when the first of them starts. It turns completed
//! when every one of them finished its task and left nothing to merge: a
//! worktree with changes counts once merge_agent applied it, best-of-N
//! attempts once one of them was applied. An agent that failed, was stopped
//! or whose worktree was discarded never completes a step; the model does.
//! The engine only moves a step forward, and only when one of its agents
//! changed. The model's update_plan wins where it changes a step: a step it
//! set back is moved again only by agents started after that. A step it
//! sends as it knew it, whose status the agents changed since its last call,
//! keeps that status (`merge_model_plan`), and the call's result says so.

use crate::event::PlanItem;

/// A subagent of the main agent, as the plan's progress reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StepAgent {
    pub path: String,
    pub task_name: String,
    pub plan_step: Option<String>,
    /// Its lifecycle status (`SubagentStatus::as_str`).
    pub status: String,
    /// It has a worktree that merge_agent has not applied or discarded.
    pub worktree: bool,
    pub attempt_group: Option<String>,
    /// When it (last) started, in ms since the epoch.
    pub started_at_ms: u64,
}

/// The step `agent` works on: its plan_step, else the step whose `agent`
/// names it (its path or task name).
pub(crate) fn step_of(agent: &StepAgent, plan: &[PlanItem]) -> Option<usize> {
    agent
        .plan_step
        .as_ref()
        .and_then(|step| plan.iter().position(|item| item.step == *step))
        .or_else(|| {
            plan.iter().position(|item| {
                item.agent
                    .as_deref()
                    .is_some_and(|name| name == agent.path || name == agent.task_name)
            })
        })
}

fn live(agent: &StepAgent) -> bool {
    matches!(agent.status.as_str(), "pending" | "running")
}

/// It works on the step, or did its part: running, finished, merged, or
/// waiting on a conflict to be resolved.
fn started(agent: &StepAgent) -> bool {
    matches!(
        agent.status.as_str(),
        "running" | "completed" | "merged" | "conflict"
    )
}

/// Whether an agent, or a best-of-N group of attempts, did its part of the
/// step: it finished and nothing of it waits to be merged (`has_changes`:
/// its worktree holds changes); a group once one attempt was applied and
/// none still runs.
fn unit_done(members: &[&StepAgent], has_changes: &impl Fn(&str) -> bool) -> bool {
    if members.iter().any(|agent| live(agent)) {
        return false;
    }
    if members.len() > 1 {
        return members.iter().any(|agent| agent.status == "merged");
    }
    members
        .first()
        .is_some_and(|agent| match agent.status.as_str() {
            "merged" => true,
            "completed" => !(agent.worktree && has_changes(&agent.path)),
            _ => false,
        })
}

/// Applies what the lifecycle changes of `changed` (agent paths) prove to
/// the steps of those agents. `counts(agent, step)` says whether the agent
/// is evidence for the step (not when it started before the model set the
/// step back); `has_changes(path)` whether a finished agent's worktree holds
/// changes still to merge (asked only of those). True when the plan changed.
pub(crate) fn advance(
    plan: &mut [PlanItem],
    agents: &[StepAgent],
    changed: &[String],
    counts: impl Fn(&StepAgent, &str) -> bool,
    has_changes: impl Fn(&str) -> bool,
) -> bool {
    let mut steps: Vec<usize> = Vec::new();
    for path in changed {
        if let Some(at) = agents
            .iter()
            .find(|agent| agent.path == *path)
            .and_then(|agent| step_of(agent, plan))
        {
            if !steps.contains(&at) {
                steps.push(at);
            }
        }
    }
    let mut moved = false;
    for at in steps {
        let step = plan[at].step.clone();
        let evidence: Vec<&StepAgent> = agents
            .iter()
            .filter(|agent| step_of(agent, plan) == Some(at) && counts(agent, &step))
            .collect();
        if evidence.is_empty() {
            continue;
        }
        let mut units: Vec<(String, Vec<&StepAgent>)> = Vec::new();
        for agent in &evidence {
            let key = agent
                .attempt_group
                .clone()
                .unwrap_or_else(|| agent.path.clone());
            match units.iter_mut().find(|(known, _)| *known == key) {
                Some((_, members)) => members.push(agent),
                None => units.push((key, vec![agent])),
            }
        }
        let status = if units
            .iter()
            .all(|(_, members)| unit_done(members, &has_changes))
        {
            "completed"
        } else if evidence.iter().any(|agent| started(agent)) {
            "in_progress"
        } else {
            continue;
        };
        let item = &mut plan[at];
        // Forward only: to do → in progress → completed.
        let forward = match status {
            "completed" => item.status != "completed",
            _ => item.status == "pending",
        };
        if forward {
            item.status = status.to_string();
            moved = true;
        }
    }
    moved
}

fn rank(status: &str) -> u8 {
    match status {
        "completed" => 2,
        "in_progress" => 1,
        _ => 0,
    }
}

/// What the model's update_plan (`plan`) did against its previous call
/// (`previous`) and the plan now (`current`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ModelPlan {
    /// Steps it sent as it last sent them, whose status their agents changed
    /// since: they keep that status (indices into the plan).
    pub kept: Vec<usize>,
    /// Steps it set back from the status they have now (their text).
    pub set_back: Vec<String>,
}

/// `previous` is the plan as the model knows it: what its last call left
/// (with what that call's result said was kept).
pub(crate) fn merge_model_plan(
    plan: &mut [PlanItem],
    previous: &[PlanItem],
    current: &[PlanItem],
) -> ModelPlan {
    let mut out = ModelPlan::default();
    for (at, item) in plan.iter_mut().enumerate() {
        let Some(now) = current.iter().find(|known| known.step == item.step) else {
            continue;
        };
        let unchanged = previous
            .iter()
            .find(|known| known.step == item.step)
            .is_some_and(|before| before.status == item.status);
        if unchanged {
            if now.status != item.status {
                item.status = now.status.clone();
                out.kept.push(at);
            }
        } else if rank(&item.status) < rank(&now.status) {
            out.set_back.push(item.step.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(step: &str, status: &str) -> PlanItem {
        PlanItem {
            step: step.to_string(),
            status: status.to_string(),
            agent: None,
            files: Vec::new(),
        }
    }

    fn plan() -> Vec<PlanItem> {
        vec![
            item("Scaffold", "completed"),
            item("Cards", "pending"),
            item("Enemies", "pending"),
        ]
    }

    fn agent(name: &str, step: &str, status: &str) -> StepAgent {
        StepAgent {
            path: format!("/root/{name}"),
            task_name: name.to_string(),
            plan_step: Some(step.to_string()),
            status: status.to_string(),
            ..StepAgent::default()
        }
    }

    fn statuses(plan: &[PlanItem]) -> Vec<&str> {
        plan.iter().map(|item| item.status.as_str()).collect()
    }

    /// `advance` where every agent counts and a worktree has changes.
    fn run(plan: &mut [PlanItem], agents: &[StepAgent], changed: &[&str]) -> bool {
        let changed: Vec<String> = changed.iter().map(|path| path.to_string()).collect();
        advance(plan, agents, &changed, |_, _| true, |_| true)
    }

    #[test]
    fn a_step_starts_with_its_first_agent_and_completes_with_its_last() {
        let mut plan = plan();
        let mut agents = vec![
            agent("a", "Cards", "running"),
            agent("b", "Cards", "pending"),
        ];
        // Reserved only: nothing has started yet.
        assert!(!run(&mut plan, &agents[1..], &["/root/b"]));
        assert!(run(&mut plan, &agents, &["/root/a"]));
        assert_eq!(statuses(&plan), ["completed", "in_progress", "pending"]);
        agents[0].status = "completed".to_string();
        agents[1].status = "running".to_string();
        assert!(!run(&mut plan, &agents, &["/root/a", "/root/b"]));
        agents[1].status = "completed".to_string();
        assert!(run(&mut plan, &agents, &["/root/b"]));
        assert_eq!(statuses(&plan), ["completed", "completed", "pending"]);
    }

    #[test]
    fn a_failed_or_discarded_agent_never_completes_its_step() {
        for status in [
            "errored",
            "interrupted",
            "closed",
            "budget_exhausted",
            "discarded",
            "conflict",
        ] {
            let mut plan = plan();
            plan[1].status = "in_progress".to_string();
            let agents = vec![
                agent("a", "Cards", "completed"),
                agent("b", "Cards", status),
            ];
            assert!(!run(&mut plan, &agents, &["/root/b"]), "{status}");
            assert_eq!(plan[1].status, "in_progress", "{status}");
        }
        // An agent that failed at once does not start its step either.
        let mut plan = plan();
        assert!(!run(
            &mut plan,
            &[agent("a", "Cards", "errored")],
            &["/root/a"]
        ));
        assert_eq!(plan[1].status, "pending");
    }

    #[test]
    fn a_worktree_with_changes_completes_its_step_once_merged() {
        let mut plan = plan();
        let mut cards = agent("a", "Cards", "completed");
        cards.worktree = true;
        let mut agents = vec![cards];
        assert!(run(&mut plan, &agents, &["/root/a"]));
        assert_eq!(plan[1].status, "in_progress");
        agents[0].status = "merged".to_string();
        agents[0].worktree = false;
        assert!(run(&mut plan, &agents, &["/root/a"]));
        assert_eq!(plan[1].status, "completed");
        // A worktree that changed nothing has nothing to merge.
        let mut plan = super::tests::plan();
        let mut enemies = agent("b", "Enemies", "completed");
        enemies.worktree = true;
        let changed = ["/root/b".to_string()];
        assert!(advance(
            &mut plan,
            &[enemies],
            &changed,
            |_, _| true,
            |_| false
        ));
        assert_eq!(plan[2].status, "completed");
    }

    #[test]
    fn attempts_complete_their_step_once_one_is_applied() {
        let attempt = |n: u64, status: &str| StepAgent {
            attempt_group: Some("fix".to_string()),
            worktree: status == "completed",
            ..agent(&format!("fix_a{n}"), "Cards", status)
        };
        let mut plan = plan();
        let agents = vec![attempt(1, "completed"), attempt(2, "running")];
        assert!(run(&mut plan, &agents, &["/root/fix_a1"]));
        assert_eq!(plan[1].status, "in_progress");
        let agents = vec![attempt(1, "completed"), attempt(2, "completed")];
        assert!(!run(&mut plan, &agents, &["/root/fix_a2"]));
        // pick_attempt: one applied, the others discarded or failed.
        let agents = vec![
            attempt(1, "discarded"),
            attempt(2, "merged"),
            attempt(3, "errored"),
        ];
        assert!(run(&mut plan, &agents, &["/root/fix_a2"]));
        assert_eq!(plan[1].status, "completed");
    }

    #[test]
    fn a_step_names_its_agent_and_others_are_left_alone() {
        let mut plan = plan();
        plan[2].agent = Some("enemies".to_string());
        let named = StepAgent {
            plan_step: None,
            ..agent("enemies", "", "running")
        };
        let stray = StepAgent {
            plan_step: Some("Deploy".to_string()),
            ..agent("stray", "", "completed")
        };
        let loose = StepAgent {
            plan_step: None,
            ..agent("loose", "", "completed")
        };
        assert!(run(
            &mut plan,
            &[named, stray, loose],
            &["/root/enemies", "/root/stray", "/root/loose"]
        ));
        assert_eq!(statuses(&plan), ["completed", "pending", "in_progress"]);
    }

    #[test]
    fn only_forward_and_only_for_the_steps_of_agents_that_changed() {
        // A step the model completed stays completed when its agent fails.
        let mut plan = plan();
        plan[1].status = "completed".to_string();
        assert!(!run(
            &mut plan,
            &[agent("a", "Cards", "errored")],
            &["/root/a"]
        ));
        assert_eq!(plan[1].status, "completed");
        // A step is moved only when one of its agents changed.
        let mut plan = super::tests::plan();
        let agents = [
            agent("a", "Cards", "completed"),
            agent("b", "Enemies", "running"),
        ];
        assert!(run(&mut plan, &agents, &["/root/b"]));
        assert_eq!(statuses(&plan), ["completed", "pending", "in_progress"]);
        // A step the model set back: agents from before that are no evidence.
        let mut plan = super::tests::plan();
        let old = StepAgent {
            started_at_ms: 100,
            ..agent("a", "Cards", "merged")
        };
        let changed = ["/root/a".to_string()];
        let after = |agent: &StepAgent, step: &str| step != "Cards" || agent.started_at_ms > 200;
        assert!(!advance(
            &mut plan,
            std::slice::from_ref(&old),
            &changed,
            after,
            |_| true
        ));
        assert_eq!(plan[1].status, "pending");
        let new = StepAgent {
            started_at_ms: 300,
            ..agent("b", "Cards", "running")
        };
        let changed = ["/root/b".to_string()];
        assert!(advance(&mut plan, &[old, new], &changed, after, |_| true));
        assert_eq!(plan[1].status, "in_progress");
    }

    #[test]
    fn the_model_wins_where_it_changed_a_step_and_is_kept_where_it_did_not() {
        let previous = plan();
        let mut current = plan();
        current[1].status = "completed".to_string();
        current[2].status = "in_progress".to_string();
        // The model sends its old statuses for steps 2 and 3, and changes
        // step 1, and adds a step.
        let mut sent = vec![
            item("Scaffold", "in_progress"),
            item("Cards", "pending"),
            item("Enemies", "pending"),
            item("Polish", "pending"),
        ];
        let merged = merge_model_plan(&mut sent, &previous, &current);
        assert_eq!(
            merged,
            ModelPlan {
                kept: vec![1, 2],
                set_back: vec!["Scaffold".to_string()]
            }
        );
        assert_eq!(
            statuses(&sent),
            ["in_progress", "completed", "in_progress", "pending"]
        );
        // A step it changes wins, even back to pending.
        let mut sent = vec![
            item("Scaffold", "completed"),
            item("Cards", "in_progress"),
            item("Enemies", "pending"),
        ];
        let merged = merge_model_plan(&mut sent, &previous, &current);
        assert_eq!(
            merged,
            ModelPlan {
                kept: vec![2],
                set_back: vec!["Cards".to_string()]
            }
        );
        assert_eq!(statuses(&sent), ["completed", "in_progress", "in_progress"]);
        // Told what was kept, the model sends the old status again: its own.
        let mut sent = vec![
            item("Scaffold", "completed"),
            item("Cards", "pending"),
            item("Enemies", "pending"),
        ];
        let merged = merge_model_plan(&mut sent, &current, &current);
        assert_eq!(
            merged,
            ModelPlan {
                kept: vec![],
                set_back: vec!["Cards".to_string(), "Enemies".to_string()]
            }
        );
        assert_eq!(statuses(&sent), ["completed", "pending", "pending"]);
        // Its first plan is its own; a step moved forward is not set back.
        let mut sent = vec![item("Cards", "pending"), item("Enemies", "completed")];
        let merged = merge_model_plan(&mut sent, &[], &current);
        assert_eq!(
            merged,
            ModelPlan {
                kept: vec![],
                set_back: vec!["Cards".to_string()]
            }
        );
        assert_eq!(statuses(&sent), ["pending", "completed"]);
    }
}
