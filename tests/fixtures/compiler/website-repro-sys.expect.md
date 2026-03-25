## Input

```javascript
// Reduced from the website terminal chunk.
// Reproduces exact output drift centered on switch-heavy command handling
// and emitted memo-cache layout.
import { useCallback, useRef } from 'react';

interface Result {
  type: 'output' | 'error' | 'clear' | 'exit' | 'diagnostic';
  content?: string;
  message?: string;
  needsConfirmation?: boolean;
}

interface Props {
  input: string;
  items: string[];
  executeCommand(input: string): Result;
  terminalExecute(input: string, content: string | JSX.Element | null, isError?: boolean): void;
  clear(): void;
  awaitConfirmation(value: string): void;
  addOutput(node: JSX.Element): void;
  handleNavigateHome(): void;
  showDiagnostic(): void;
}

export default function TerminalSwitchReduction(props: Props) {
  const navigateRef = useRef(props.handleNavigateHome);

  const handleSubmit = useCallback(() => {
    const result = props.executeCommand(props.input);

    switch (result.type) {
      case 'output':
        props.terminalExecute(props.input, result.content ?? null);
        break;
      case 'error':
        props.terminalExecute(props.input, <span>{result.message}</span>, true);
        break;
      case 'clear':
        props.clear();
        break;
      case 'exit':
        if (result.needsConfirmation) {
          props.awaitConfirmation('exit');
          props.addOutput(<span>exit? (y/n)</span>);
        } else {
          navigateRef.current();
        }
        break;
      case 'diagnostic':
        props.terminalExecute(props.input, null);
        props.showDiagnostic();
        break;
    }
  }, [props]);

  return (
    <div>
      {props.items.map(item => (
        <span key={item}>{item}</span>
      ))}
      <button onClick={handleSubmit} type='button'>
        run
      </button>
    </div>
  );
}
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Reduced from the website terminal chunk.
// Reproduces exact output drift centered on switch-heavy command handling
// and emitted memo-cache layout.
import { useCallback, useRef } from 'react';
interface Result {
  type: 'output' | 'error' | 'clear' | 'exit' | 'diagnostic';
  content?: string;
  message?: string;
  needsConfirmation?: boolean;
}
interface Props {
  input: string;
  items: string[];
  executeCommand(input: string): Result;
  terminalExecute(input: string, content: string | JSX.Element | null, isError?: boolean): void;
  clear(): void;
  awaitConfirmation(value: string): void;
  addOutput(node: JSX.Element): void;
  handleNavigateHome(): void;
  showDiagnostic(): void;
}
export default function TerminalSwitchReduction(props) {
  const $ = _c(9);
  const navigateRef = useRef(props.handleNavigateHome);
  let t0;
  if ($[0] !== props) {
    t0 = () => {
      const result = props.executeCommand(props.input);
      bb2: switch (result.type) {
        case "output":
          {
            props.terminalExecute(props.input, result.content ?? null);
            break bb2;
          }
        case "error":
          {
            props.terminalExecute(props.input, <span>{result.message}</span>, true);
            break bb2;
          }
        case "clear":
          {
            props.clear();
            break bb2;
          }
        case "exit":
          {
            if (result.needsConfirmation) {
              props.awaitConfirmation("exit");
              props.addOutput(<span>exit? (y/n)</span>);
            } else {
              navigateRef.current();
            }
            break bb2;
          }
        case "diagnostic":
          {
            props.terminalExecute(props.input, null);
            props.showDiagnostic();
          }
      }
    };
    $[0] = props;
    $[1] = t0;
  } else {
    t0 = $[1];
  }
  const handleSubmit = t0;
  let t1;
  if ($[2] !== props.items) {
    t1 = props.items.map(_temp);
    $[2] = props.items;
    $[3] = t1;
  } else {
    t1 = $[3];
  }
  let t2;
  if ($[4] !== handleSubmit) {
    t2 = <button onClick={handleSubmit} type="button">run</button>;
    $[4] = handleSubmit;
    $[5] = t2;
  } else {
    t2 = $[5];
  }
  let t3;
  if ($[6] !== t1 || $[7] !== t2) {
    t3 = <div>{t1}{t2}</div>;
    $[6] = t1;
    $[7] = t2;
    $[8] = t3;
  } else {
    t3 = $[8];
  }
  return t3;
}
function _temp(item) {
  return <span key={item}>{item}</span>;
}
```
