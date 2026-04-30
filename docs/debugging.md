# Debugging

ƒink ships with a built-in debug adapter: `fink dap <file>` speaks the Debug Adapter Protocol on stdin/stdout, so any DAP client can drive a ƒink program.

## VSCode

The ƒink VSCode extension (linked from the repo root [README](../README.md)) registers `fink` as a debugger type. With the extension installed, a launch config looks like:

```text
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "fink",
      "request": "launch",
      "name": "Debug ƒink",
      "program": "${file}",
      "stopOnEntry": false
    }
  ]
}
```

Save as `.vscode/launch.json` in the workspace. Set `stopOnEntry: true` to pause on the first expression of the program; leave it `false` to run from start to the first breakpoint.

## What works

- **Breakpoints.** Click in the gutter to set one. A breakpoint on a line with no executable ƒink expression comes back unverified (greyed out) — move it to the nearest line with a binding, call, or operator.
- **Continue.** Runs until the next breakpoint or program end.
- **Step in / over / out.** All three advance to the next executable ƒink expression. True step-over and step-out (skip-the-callee, run-to-return) are a known limitation — every ƒink call is a tail call, so there is no call stack for the debugger to walk.
- **Program output.** `write stdout, 'hello'` / `write stderr, ...` shows up in the editor's debug console.
- **Clean exit.** The debug session ends when `main` returns.

## Running from the command line

```bash
fink dap path/to/program.fnk
```

Reads DAP requests on stdin, writes events and responses on stdout. Useful if you're wiring ƒink into a different editor, or writing an integration test.

## Known gaps

- **Stdin inside the debugger isn't implemented.** A program that reads from its `stdin` channel while running under `fink dap` will trap. `fink run` works normally.
- **Panic messages carry no source location.** A runtime panic (e.g. an irrefutable pattern failing) traps with a generic message rather than pointing at the line.
