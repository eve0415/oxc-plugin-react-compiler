## Input

```javascript
// Reduced from /tmp/website/src/routes/sys/-components/Terminal/terminal.tsx
// Real-world drift: Babel and OXC differ in the emitted control-flow shape for
// switch-heavy command handlers in callbacks.
function Component({
  input,
  executeCommand,
  terminalExecute,
  clear,
  awaitConfirmation,
  addOutput,
  handleNavigateHome,
  showDiagnostic,
}) {
  const handleSubmit = () => {
    const result = executeCommand(input);

    switch (result.type) {
      case 'output':
        terminalExecute(input, result.content);
        break;
      case 'error':
        terminalExecute(input, <span className="text-red-400">{result.message}</span>, true);
        break;
      case 'clear':
        clear();
        break;
      case 'exit':
        if (result.needsConfirmation) {
          awaitConfirmation('exit');
          addOutput(<span>exit? (y/n)</span>);
        } else {
          handleNavigateHome();
        }
        break;
      case 'diagnostic':
        terminalExecute(input, null);
        showDiagnostic();
        break;
    }
  };

  return <button onClick={handleSubmit}>Run</button>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [
    {
      input: 'ls',
      executeCommand: () => ({ type: 'output', content: 'ok' }),
      terminalExecute: () => {},
      clear: () => {},
      awaitConfirmation: () => {},
      addOutput: () => {},
      handleNavigateHome: () => {},
      showDiagnostic: () => {},
    },
  ],
};
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Reduced from /tmp/website/src/routes/sys/-components/Terminal/terminal.tsx
// Real-world drift: Babel and OXC differ in the emitted control-flow shape for
// switch-heavy command handlers in callbacks.
function Component(t0) {
  const $ = _c(9);
  const {
    input,
    executeCommand,
    terminalExecute,
    clear,
    awaitConfirmation,
    addOutput,
    handleNavigateHome,
    showDiagnostic
  } = t0;
  let t1;
  if ($[0] !== addOutput || $[1] !== awaitConfirmation || $[2] !== clear || $[3] !== executeCommand || $[4] !== handleNavigateHome || $[5] !== input || $[6] !== showDiagnostic || $[7] !== terminalExecute) {
    const handleSubmit = () => {
      const result = executeCommand(input);
      bb2: switch (result.type) {
        case "output":
          {
            terminalExecute(input, result.content);
            break bb2;
          }
        case "error":
          {
            terminalExecute(input, <span className="text-red-400">{result.message}</span>, true);
            break bb2;
          }
        case "clear":
          {
            clear();
            break bb2;
          }
        case "exit":
          {
            if (result.needsConfirmation) {
              awaitConfirmation("exit");
              addOutput(<span>exit? (y/n)</span>);
            } else {
              handleNavigateHome();
            }
            break bb2;
          }
        case "diagnostic":
          {
            terminalExecute(input, null);
            showDiagnostic();
          }
      }
    };
    t1 = <button onClick={handleSubmit}>Run</button>;
    $[0] = addOutput;
    $[1] = awaitConfirmation;
    $[2] = clear;
    $[3] = executeCommand;
    $[4] = handleNavigateHome;
    $[5] = input;
    $[6] = showDiagnostic;
    $[7] = terminalExecute;
    $[8] = t1;
  } else {
    t1 = $[8];
  }
  return t1;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    input: 'ls',
    executeCommand: () => ({
      type: 'output',
      content: 'ok'
    }),
    terminalExecute: () => {},
    clear: () => {},
    awaitConfirmation: () => {},
    addOutput: () => {},
    handleNavigateHome: () => {},
    showDiagnostic: () => {}
  }]
};
```
