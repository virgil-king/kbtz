use crate::project::OrchestratorState;
use crate::step::StepPhase;
use std::path::Path;

const LEADER_SYSTEM_DEFAULT: &str = include_str!("../prompts/leader-system.md");
const IMPLEMENTATION_DEFAULT: &str = include_str!("../prompts/implementation.md");
const STAKEHOLDER_DEFAULT: &str = include_str!("../prompts/stakeholder.md");

/// Load a prompt from the project directory if it exists, otherwise use the default.
fn load_prompt(project_dir: Option<&Path>, filename: &str, default: &str) -> String {
    if let Some(dir) = project_dir {
        let path = dir.join("prompts").join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            return content;
        }
    }
    default.to_string()
}

/// System prompt for the leader session.
pub fn leader_system_prompt_from(project_dir: Option<&Path>) -> String {
    load_prompt(project_dir, "leader-system.md", LEADER_SYSTEM_DEFAULT)
}

/// System prompt for the leader session (default, no project override).
pub fn leader_system_prompt() -> String {
    LEADER_SYSTEM_DEFAULT.to_string()
}

/// Build the headless leader prompt with full state snapshot and feedback.
pub fn leader_decision_prompt(
    state: &OrchestratorState,
    step_feedback: &[(String, Vec<(String, String)>)],
    project_md: Option<&str>,
) -> String {
    let mut prompt = String::new();

    if let Some(md) = project_md {
        prompt.push_str("# Project Definition\n\n");
        prompt.push_str(md);
        prompt.push_str("\n\n---\n\n");
    }

    prompt.push_str("# Current Project State\n\n");
    prompt.push_str(&format!("**Goal:** {}\n\n", state.project.goal_summary));

    prompt.push_str("**Repos:**\n");
    for repo in &state.project.repos {
        prompt.push_str(&format!("- {} ({})\n", repo.name, repo.url));
    }

    prompt.push_str("\n**All Steps:**\n");
    for step in &state.steps {
        let phase_str = match &step.phase {
            StepPhase::Dispatched => "dispatched",
            StepPhase::Running => "running",
            StepPhase::Completed => "completed",
            StepPhase::Reviewing => "reviewing",
            StepPhase::Reviewed => "REVIEWED -- needs your action",
            StepPhase::Merged => "merged",
            StepPhase::Rework => "rework in progress",
        };
        prompt.push_str(&format!(
            "- {} [{}]: {}\n",
            step.id, phase_str, step.dispatch.prompt
        ));
    }

    if !step_feedback.is_empty() {
        prompt.push_str("\n# Steps Ready for Your Review\n\n");
        for (step_id, feedbacks) in step_feedback {
            prompt.push_str(&format!("## {}\n\n", step_id));
            prompt.push_str(&format!(
                "Branch `{}` has been fetched into your repos.\n\n",
                step_id
            ));
            for (stakeholder, feedback) in feedbacks {
                prompt.push_str(&format!("### {} feedback\n{}\n\n", stakeholder, feedback));
            }
        }
        prompt.push_str("Review the feedback above. For each step, either:\n");
        prompt.push_str("1. Merge the branch in your repos and call close_step(step_id)\n");
        prompt.push_str(
            "2. Call rework_step(step_id, feedback) with specific changes needed\n\n",
        );
        prompt.push_str("You may also dispatch new follow-up steps if needed.\n");
    }

    prompt
}

/// Build the prompt for an implementation agent session.
pub fn implementation_prompt(project_dir: Option<&Path>, step_prompt: &str) -> String {
    let template = load_prompt(project_dir, "implementation.md", IMPLEMENTATION_DEFAULT);
    template.replace("{prompt}", step_prompt)
}

/// Build the prompt for a stakeholder review session.
pub fn stakeholder_prompt(
    project_dir: Option<&Path>,
    persona: &str,
    step_prompt: &str,
    summary: &str,
) -> String {
    let template = load_prompt(project_dir, "stakeholder.md", STAKEHOLDER_DEFAULT);
    template
        .replace("{persona}", persona)
        .replace("{step_prompt}", step_prompt)
        .replace("{summary}", summary)
}
