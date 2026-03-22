## Code

```javascript
import { c as _c } from "react/compiler-runtime";
import { useEffect, useState } from "react";
function ContactForm() {
  const $ = _c(10);
  const [state, setState] = useState("idle");
  const [globalError, setGlobalError] = useState(null);
  const form = useForm({
    onSubmit: async (t0) => {
      const { value, formApi } = t0;
      setGlobalError(undefined);
      try {
        const result = await handleForm({ data: value });
        if (result.success) {
          setState("success");
          formApi.reset();
        } else {
          handleError(result);
        }
      } catch (t1) {
        const error = t1;
        console.error("Failed:", error);
        setGlobalError("Unexpected");
        setState("error");
      }
    },
  });
  let t2;
  let t3;
  if ($[0] !== state) {
    t2 = () => {
      if (state !== "success") {
        return;
      }
      const timer = setTimeout(() => setState("idle"), 5000);
      return () => clearTimeout(timer);
    };
    t3 = [state];
    $[0] = state;
    $[1] = t2;
    $[2] = t3;
  } else {
    t2 = $[1];
    t3 = $[2];
  }
  useEffect(t2, t3);
  const handleError = (result_0) => {
    setState("error");
    for (const fieldName of ["name", "email"]) {
      const error_0 = result_0.errors[fieldName];
      if (error_0) {
        form.setFieldMeta(fieldName, (prev) => ({ ...prev, errors: [error_0] }));
      }
    }
  };
  let t4;
  if ($[3] !== globalError) {
    t4 = globalError && <p>{globalError}</p>;
    $[3] = globalError;
    $[4] = t4;
  } else {
    t4 = $[4];
  }
  let t5;
  if ($[5] !== form.state.isSubmitting) {
    t5 = <button type="submit" disabled={form.state.isSubmitting}>Submit</button>;
    $[5] = form.state.isSubmitting;
    $[6] = t5;
  } else {
    t5 = $[6];
  }
  let t6;
  if ($[7] !== t4 || $[8] !== t5) {
    t6 = <form>{t4}{t5}</form>;
    $[7] = t4;
    $[8] = t5;
    $[9] = t6;
  } else {
    t6 = $[9];
  }
  return t6;
}
function useForm(opts) {
  return { state: { isSubmitting: false }, setFieldMeta: () => {} };
}
async function handleForm(data) {
  return { success: true };
}
export const FIXTURE_ENTRYPOINT = { fn: ContactForm, params: [{}] };
```
