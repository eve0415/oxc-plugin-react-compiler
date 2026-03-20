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
