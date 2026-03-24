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
