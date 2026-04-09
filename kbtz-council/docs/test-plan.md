# Manual Test Plan

## Phase 1: TUI + MCP endpoint

- [ ] Start orchestrator: `./target/debug/kbtz-council --project /tmp/test-project`
- [ ] TUI renders empty dashboard, no crash
- [ ] MCP port printed to stderr before TUI starts
- [ ] In another terminal, curl initialize:
      `curl -s -X POST http://127.0.0.1:<port>/mcp -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'`
- [ ] Curl tools/list returns 4 tools
- [ ] Curl define_project with a test goal and security stakeholder
- [ ] `state.json` shows project definition
- [ ] Curl dispatch_step with a trivial prompt and empty repos
- [ ] TUI shows the step as DISPATCHED
- [ ] `q` exits cleanly
- [ ] Restart — state persists from disk

## Phase 2: Implementation session

- [ ] Define project with a real repo (git init a throwaway)
- [ ] Dispatch a step: "Create a file called hello.txt with the text 'hello world'"
- [ ] Verify claude -p process spawns (check ps)
- [ ] TUI shows step as RUNNING
- [ ] Stream-json events appear in session panel
- [ ] Trace file appears in project/traces/
- [ ] After session exits: step transitions to COMPLETED
- [ ] Commits fetched into leader's repo as a branch

## Phase 3: Stakeholder review

- [ ] After step completes, stakeholder sessions spawn automatically
- [ ] Each stakeholder runs claude -p with persona prompt
- [ ] Feedback stored on step
- [ ] After all stakeholders exit: step transitions to REVIEWED

## Phase 4: Leader decision

- [ ] Leader session spawns automatically with full state + feedback
- [ ] Leader calls close_step or rework_step via MCP
- [ ] Step transitions to MERGED or REWORK accordingly
- [ ] If rework: new implementation session spawns with feedback context
- [ ] Session resumption works (--resume on second invocation)

## Phase 5: Interactive leader

- [ ] 'l' key spawns interactive leader PTY in TUI
- [ ] User can chat with leader, call MCP tools
- [ ] Esc returns to dashboard
- [ ] Leader session resumes on next 'l' press

## Known gaps before testing

- Interactive leader spawn not wired (Phase 5 will fail)
- MCP port only printed to stderr, may be hidden by TUI alt screen
