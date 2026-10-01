`pane restart` without `--force` no longer risks refusing a role whose agent has just exited with "has not crashed": an agent that stops on its own is recorded as crashed before it is shown as gone.
