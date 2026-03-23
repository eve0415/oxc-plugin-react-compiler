## Input

```javascript
// @compilationMode:"infer"
// Switch-case breaks in complex inner function (useCallback).
// OXC bug: suppress_labels_recursive strips the labeled block wrapper
// but leaves orphaned `break bb7;` referencing the removed label.
import { useCallback, useState } from 'react';
function Terminal() {
  const [output, setOutput] = useState('');
  const [pending, setPending] = useState(false);
  const handleCommand = useCallback((input, context) => {
    if (pending) {
      const isYes = input.toLowerCase() === 'y';
      setPending(false);
      if (isYes) context.navigate();
      return;
    }
    const result = processCommand(input, context);
    switch (result.type) {
      case 'output':
        setOutput(result.content);
        break;
      case 'error':
        setOutput('Error: ' + result.message);
        break;
      case 'clear':
        setOutput('');
        break;
      case 'exit':
        if (result.needsConfirmation) {
          setPending(true);
          setOutput('exit? (y/n)');
        } else {
          context.navigate();
        }
        break;
    }
  }, [pending]);
  return <div onClick={() => handleCommand('help', {})}>{output}</div>;
}
function processCommand(input, ctx) { return { type: 'output', content: input }; }

export const FIXTURE_ENTRYPOINT = {
  fn: Terminal,
  params: [{}],
};
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// @compilationMode:"infer"
// Switch-case breaks in complex inner function (useCallback).
// OXC bug: suppress_labels_recursive strips the labeled block wrapper
// but leaves orphaned `break bb7;` referencing the removed label.
import { useCallback, useState } from 'react';
function Terminal() {
  const $ = _c(7);
  const [output, setOutput] = useState("");
  const [pending, setPending] = useState(false);
  let t0;
  if ($[0] !== pending) {
    t0 = (input, context) => {
      if (pending) {
        const isYes = input.toLowerCase() === "y";
        setPending(false);
        if (isYes) {
          context.navigate();
        }
        return;
      }
      const result = processCommand(input, context);
      bb7: switch (result.type) {
        case "output":
          {
            setOutput(result.content);
            break bb7;
          }
        case "error":
          {
            setOutput("Error: " + result.message);
            break bb7;
          }
        case "clear":
          {
            setOutput("");
            break bb7;
          }
        case "exit":
          {
            if (result.needsConfirmation) {
              setPending(true);
              setOutput("exit? (y/n)");
            } else {
              context.navigate();
            }
          }
      }
    };
    $[0] = pending;
    $[1] = t0;
  } else {
    t0 = $[1];
  }
  const handleCommand = t0;
  let t1;
  if ($[2] !== handleCommand) {
    t1 = () => handleCommand("help", {});
    $[2] = handleCommand;
    $[3] = t1;
  } else {
    t1 = $[3];
  }
  let t2;
  if ($[4] !== output || $[5] !== t1) {
    t2 = <div onClick={t1}>{output}</div>;
    $[4] = output;
    $[5] = t1;
    $[6] = t2;
  } else {
    t2 = $[6];
  }
  return t2;
}
function processCommand(input, ctx) {
  return {
    type: 'output',
    content: input
  };
}
export const FIXTURE_ENTRYPOINT = {
  fn: Terminal,
  params: [{}]
};
```
