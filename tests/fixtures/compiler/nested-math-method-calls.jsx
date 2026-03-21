// Nested global method calls: Math.min(Math.floor(x), 100)
// OXC bails: "MethodCall::property must be an unpromoted + unmemoized MemberExpression"
import { useState } from 'react';
function Component() {
  const [progress] = useState(0);
  return <div>{Math.min(Math.floor(progress), 100)}%</div>;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{}],
};
