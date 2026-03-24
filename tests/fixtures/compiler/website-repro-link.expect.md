## Input

```javascript
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
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Reduced from the website contact-form path.
// Reproduces exact transform drift plus a real AST mismatch involving
// async submit flow, try/catch, field-meta callbacks, and switch-based
// error handling.
import { useEffect, useState } from 'react';
function TurnstileWidget(props) {
  const $ = _c(10);
  let t0;
  if ($[0] !== props) {
    t0 = <button onClick={() => props.onVerify("token")} type="button">verify</button>;
    $[0] = props;
    $[1] = t0;
  } else {
    t0 = $[1];
  }
  let t1;
  if ($[2] !== props.onError) {
    t1 = <button onClick={props.onError} type="button">error</button>;
    $[2] = props.onError;
    $[3] = t1;
  } else {
    t1 = $[3];
  }
  let t2;
  if ($[4] !== props.onExpire) {
    t2 = <button onClick={props.onExpire} type="button">expire</button>;
    $[4] = props.onExpire;
    $[5] = t2;
  } else {
    t2 = $[5];
  }
  let t3;
  if ($[6] !== t0 || $[7] !== t1 || $[8] !== t2) {
    t3 = <div>{t0}{t1}{t2}</div>;
    $[6] = t0;
    $[7] = t1;
    $[8] = t2;
    $[9] = t3;
  } else {
    t3 = $[9];
  }
  return t3;
}
interface FormFieldState {
  value: string;
  meta: {
    errors: string[];
  };
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
  Field(props: {
    name: string;
    children: (field: FormFieldApi) => JSX.Element;
  }): JSX.Element;
  setFieldMeta(name: string, update: (prev: FormFieldState['meta']) => FormFieldState['meta']): void;
  state: {
    isSubmitting: boolean;
  };
}
interface Props {
  useForm(options: {
    onSubmit(args: {
      value: Record<string, string>;
      formApi: FormApi;
    }): Promise<void>;
  }): FormController;
  handleForm(args: {
    data: FormData;
  }): Promise<ContactFormResult>;
}
export default function LinkContactFormReduction({
  useForm,
  handleForm
}: Props) {
  const [submissionState, setSubmissionState] = useState<'idle' | 'success' | 'error'>('idle');
  const [globalError, setGlobalError] = useState<string | undefined>();
  const form = useForm({
    onSubmit: async ({
      value,
      formApi
    }) => {
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
        const result = await handleForm({
          data: formData
        });
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
    }
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
  const handleError = (result_0: ContactFormResult) => {
    setSubmissionState('error');
    switch (result_0.error) {
      case 'validation':
        for (const fieldName of ['name', 'message']) {
          const error_0 = result_0.errors[fieldName];
          if (error_0) {
            form.setFieldMeta(fieldName, prev => ({
              ...prev,
              errors: [error_0]
            }));
          }
        }
        break;
      case 'turnstile':
      case 'rate_limit':
        setGlobalError(result_0.message);
        break;
    }
  };
  const isDisabled = form.state.isSubmitting || submissionState === 'success';
  return <form onSubmit={_event => {}}>
      {['name', 'message'].map(fieldName_0 => <form.Field key={fieldName_0} name={fieldName_0}>
          {field => <div>
              <input disabled={isDisabled} name={field.name} onBlur={field.handleBlur} onChange={event => {
          field.handleChange(event.target.value);
        }} onFocus={() => {
          field.setMeta(prev_0 => ({
            ...prev_0,
            errors: []
          }));
        }} value={field.state.value} />
              {field.state.meta.errors.length > 0 ? <p>{field.state.meta.errors[0]}</p> : <span />}
            </div>}
        </form.Field>)}

      <form.Field name='turnstileToken'>
        {field_0 => <div>
            <input name={field_0.name} type='hidden' value={field_0.state.value} />
            <TurnstileWidget onError={() => {
          setGlobalError('turnstile-failed');
        }} onExpire={() => {
          field_0.handleChange('');
        }} onVerify={token => {
          field_0.handleChange(token);
        }} />
          </div>}
      </form.Field>

      {globalError ? <div>{globalError}</div> : null}
    </form>;
}
```
