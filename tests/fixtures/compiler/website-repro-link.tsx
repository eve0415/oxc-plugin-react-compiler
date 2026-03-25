// @panicThreshold:"none"
// Reduced from the website contact-form path.
// Reproduces exact transform drift plus a real AST mismatch involving
// async submit flow, try/catch, field-meta callbacks, and switch-based
// error handling.
import { useEffect, useState } from 'react';

function TurnstileWidget(props: {
  onVerify: (token: string) => void;
  onError: () => void;
  onExpire: () => void;
}) {
  return (
    <div>
      <button onClick={() => props.onVerify('token')} type='button'>
        verify
      </button>
      <button onClick={props.onError} type='button'>
        error
      </button>
      <button onClick={props.onExpire} type='button'>
        expire
      </button>
    </div>
  );
}

interface FormFieldState {
  value: string;
  meta: { errors: string[] };
}

interface FormFieldApi {
  name: string;
  state: FormFieldState;
  handleChange(value: string): void;
  handleBlur(): void;
  setMeta(update: (prev: FormFieldState['meta']) => FormFieldState['meta']): void;
}

interface FormApi {
  reset(): void;
}

interface ContactFormResult {
  success?: boolean;
  error?: 'validation' | 'turnstile' | 'rate_limit';
  message?: string;
  errors: Record<string, string | undefined>;
}

interface FormController {
  Field(props: { name: string; children: (field: FormFieldApi) => JSX.Element }): JSX.Element;
  setFieldMeta(name: string, update: (prev: FormFieldState['meta']) => FormFieldState['meta']): void;
  state: { isSubmitting: boolean };
}

interface Props {
  useForm(options: {
    onSubmit(args: { value: Record<string, string>; formApi: FormApi }): Promise<void>;
  }): FormController;
  handleForm(args: { data: FormData }): Promise<ContactFormResult>;
}

export default function LinkContactFormReduction({ useForm, handleForm }: Props) {
  const [submissionState, setSubmissionState] = useState<'idle' | 'success' | 'error'>('idle');
  const [globalError, setGlobalError] = useState<string | undefined>();

  const form = useForm({
    onSubmit: async ({ value, formApi }) => {
      setGlobalError(undefined);
      if (!value.turnstileToken) {
        setGlobalError('verify');
        return;
      }

      try {
        const formData = new FormData();
        formData.append('name', value.name);
        formData.append('message', value.message);
        formData.append('turnstileToken', value.turnstileToken);
        const result = await handleForm({ data: formData });

        if (result.success) {
          setSubmissionState('success');
          formApi.reset();
        } else if ('error' in result) {
          handleError(result);
        }
      } catch (error) {
        console.error(error);
        setGlobalError('unexpected');
        setSubmissionState('error');
      }
    },
  });

  useEffect(() => {
    if (submissionState !== 'success') return;
    const timer = setTimeout(() => {
      setSubmissionState('idle');
    }, 5000);
    return () => {
      clearTimeout(timer);
    };
  }, [submissionState]);

  const handleError = (result: ContactFormResult) => {
    setSubmissionState('error');

    switch (result.error) {
      case 'validation':
        for (const fieldName of ['name', 'message']) {
          const error = result.errors[fieldName];
          if (error) {
            form.setFieldMeta(fieldName, prev => ({ ...prev, errors: [error] }));
          }
        }
        break;
      case 'turnstile':
      case 'rate_limit':
        setGlobalError(result.message);
        break;
    }
  };

  const isDisabled = form.state.isSubmitting || submissionState === 'success';

  return (
    <form onSubmit={_event => {}}>
      {['name', 'message'].map(fieldName => (
        <form.Field key={fieldName} name={fieldName}>
          {field => (
            <div>
              <input
                disabled={isDisabled}
                name={field.name}
                onBlur={field.handleBlur}
                onChange={event => {
                  field.handleChange(event.target.value);
                }}
                onFocus={() => {
                  field.setMeta(prev => ({ ...prev, errors: [] }));
                }}
                value={field.state.value}
              />
              {field.state.meta.errors.length > 0 ? <p>{field.state.meta.errors[0]}</p> : <span />}
            </div>
          )}
        </form.Field>
      ))}

      <form.Field name='turnstileToken'>
        {field => (
          <div>
            <input name={field.name} type='hidden' value={field.state.value} />
            <TurnstileWidget
              onError={() => {
                setGlobalError('turnstile-failed');
              }}
              onExpire={() => {
                field.handleChange('');
              }}
              onVerify={token => {
                field.handleChange(token);
              }}
            />
          </div>
        )}
      </form.Field>

      {globalError ? <div>{globalError}</div> : null}
    </form>
  );
}
