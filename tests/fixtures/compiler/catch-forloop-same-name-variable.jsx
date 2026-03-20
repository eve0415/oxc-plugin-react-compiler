// Same-name variable in catch clause and for-loop body: `catch (error)` +
// `const error = result.errors[fieldName]` in a sibling arrow function.
// OXC bails: "Expected all references to a variable to be consistently
// local or context references"
import { useEffect, useState } from 'react';
function ContactForm() {
  const [state, setState] = useState('idle');
  const [globalError, setGlobalError] = useState(null);

  const form = useForm({
    onSubmit: async ({ value, formApi }) => {
      setGlobalError(undefined);
      try {
        const result = await handleForm({ data: value });
        if (result.success) {
          setState('success');
          formApi.reset();
        } else handleError(result);
      } catch (error) {
        console.error('Failed:', error);
        setGlobalError('Unexpected');
        setState('error');
      }
    },
  });

  useEffect(() => {
    if (state !== 'success') return;
    const timer = setTimeout(() => setState('idle'), 5000);
    return () => clearTimeout(timer);
  }, [state]);

  const handleError = (result) => {
    setState('error');
    for (const fieldName of ['name', 'email']) {
      const error = result.errors[fieldName];
      if (error) form.setFieldMeta(fieldName, prev => ({ ...prev, errors: [error] }));
    }
  };

  return (
    <form>
      {globalError && <p>{globalError}</p>}
      <button type="submit" disabled={form.state.isSubmitting}>Submit</button>
    </form>
  );
}
function useForm(opts) { return { state: { isSubmitting: false }, setFieldMeta: () => {} }; }
async function handleForm(data) { return { success: true }; }

export const FIXTURE_ENTRYPOINT = {
  fn: ContactForm,
  params: [{}],
};
