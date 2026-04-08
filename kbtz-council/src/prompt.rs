use crate::project::OrchestratorState;
use crate::step::StepPhase;

/// System prompt for the leader session.
pub fn leader_system_prompt() -> String {
    r#"You are the leader of an AI agent orchestration project. You have MCP tools
to manage the project:

- define_project(repos, stakeholders, goal_summary): Register the repos and
  stakeholder reviewers for this project. Call this first.
- dispatch_step(prompt, repos, files): Dispatch an implementation step.
  Describe what the implementation agent should do in the prompt. Specify
  which repos are relevant. Returns a step ID.
- rework_step(step_id, feedback): Send a completed step back to the
  implementation agent with feedback for changes.
- close_step(step_id): Close a step after you've merged its changes (or
  decided to abandon it). This cleans up the session directory.

After defining the project, save the project definition to project.md in
your working directory. Read this file at the start of every session to
recall the project state.

When invoked with feedback, review all stakeholder feedback, form your own
judgment, then either merge the implementation branch and call close_step,
or call rework_step with specific feedback. You can also dispatch new
follow-up steps.

Merge implementation branches using git merge or git cherry-pick in the
repos under repos/. Resolve any conflicts."#
        .to_string()
}

/// Build the headless leader prompt with full state snapshot and feedback.
/// `project_md` is the contents of project.md if it exists.
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
pub fn implementation_prompt(step_prompt: &str) -> String {
    format!(
        r#"You are an implementation agent. Your task:

{}

Your working directory contains the repo(s) you need to modify. If there are
multiple repos, they are in subdirectories. A `files/` directory may contain
additional context from the project leader.

Do the work, commit your changes, and provide a summary of what you did.
Make clean, focused commits. Do not push to any remote."#,
        step_prompt
    )
}

/// Build the prompt for a stakeholder review session.
pub fn stakeholder_prompt(persona: &str, step_prompt: &str, summary: &str) -> String {
    format!(
        r#"You are a code reviewer with the following focus:

{}

## Step being reviewed

**Task:** {}

**Implementation summary:** {}

Review the implementation. The repo clones are available in the working
directory for you to read code and commit history. The leader's full repos
are also available for broader context.

Provide structured feedback: what's good, what needs changes, and any
blocking concerns. Be specific -- reference file paths and line numbers."#,
        persona, step_prompt, summary
    )
}
