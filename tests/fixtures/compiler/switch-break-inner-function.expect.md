## Code

```javascript
import { c as _c } from "react/compiler-runtime";
import { useCallback, useState } from "react";
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
        case "output": {
          setOutput(result.content);
          break bb7;
        }
        case "error": {
          setOutput("Error: " + result.message);
          break bb7;
        }
        case "clear": {
          setOutput("");
          break bb7;
        }
        case "exit": {
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
  return { type: "output", content: input };
}
export const FIXTURE_ENTRYPOINT = { fn: Terminal, params: [{}] };
```
