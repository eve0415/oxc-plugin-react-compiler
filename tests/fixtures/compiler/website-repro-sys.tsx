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
