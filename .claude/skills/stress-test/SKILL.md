---
name: stress-test
description: Use to stress-test jjq by coordinating parallel agents building a Go web app, exercising push, run, conflict resolution, and bug-fix workflows
---

# jjq Stress Test

Coordinate a team of parallel agents building a Go web app to exercise jjq's full workflow — push, run, conflict resolution, and bug fixes.

## Prerequisites

- Invoke the **jj-workspace-agents** skill before dispatching agents
- Familiarize yourself with the e2e test script (`jjq-test`) for reference, but don't follow it literally

## Workflow

### 1. Bootstrap the test repo

Create a new jj repo in `/tmp` with:
- A Go module and skeleton `net/http` web app
- Basic unit test(s)
- Initial changes committed
- `main` bookmark set on the initial revision as the protected trunk

### 2. Initialize jjq

Run `jjq init` with:
- `--trunk main`
- `--check` set to a command that runs tests and lints (e.g., `go test ./... && go vet ./...`)

### 3. Plan 3 features

Come up with 3 simple features to add to the app. Track them with the task list. Deliberately engineer at least one pair of features that will cause a **merge conflict** when landed — e.g., two features that modify the same handler or route.

### 4. Dispatch 3 parallel agents

Follow the **jj-workspace-agents** skill for workspace setup. Each agent:
- Operates in its own jj workspace, parented to the `main` bookmark
- Follows strict TDD (write failing test first, then implement)
- Has a clear, verifiable definition of DONE
- Knows how to use jj correctly (use the agent prompt template from jj-workspace-agents)
- Files a "PR" when done by calling `jjq push` on their change

### 5. Manage the queue

You are the traffic cop:
- Monitor agent progress
- Run `jjq run` or `jjq run --all` to process the queue
- Handle failures: if a push or run fails, task the appropriate agent with resolving the issue (provide clear feedback and resolution criteria)
- Handle conflicts: when merge conflicts arise, task an agent with rebasing and resolving

### 6. Introduce and fix a bug

After the initial 3 features land:
1. Sneakily introduce a bug somewhere in the codebase
2. Task one of the agents with finding and fixing it via TDD (write a failing test that exposes the bug, then fix it)
3. The fix goes through the same `jjq push` → `jjq run` workflow

### 7. Observe and report

This is your **meta-task**. Throughout the entire process, note:

- **UX friction** — repeated errors, confusing output, back-and-forth with the tool
- **Correctness issues** — any case where jjq produces wrong results (critical)
- **Missing affordances** — things agents needed to do that jjq made hard
- **What worked well** — patterns that were smooth and intuitive

Deliver a summary of observations at the end.

## Agent Guidelines

All agents must:
- Use jj correctly (no git commands, no staging, commit liberally)
- Follow strict TDD
- Have a verifiable definition of DONE for their task
- Run the full check command before declaring done
