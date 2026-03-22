function Component({ cond, a, b }) {
  let x;
  if (cond) {
    x = useMemo(() => a * 2, [a]);
  } else {
    x = useMemo(() => b + 1, [b]);
  }
  return <div>{x}</div>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{ cond: true, a: 5, b: 10 }],
};
