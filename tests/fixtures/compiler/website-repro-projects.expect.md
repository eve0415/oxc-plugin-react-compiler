## Input

```javascript
// Reduced from the website projects chunk.
// Reproduces exact output drift driven by helper ordering around adjacent
// JSX callback helpers.
interface Props {
  tags: string[];
  links: Array<{ label: string; url: string }>;
}

export default function ProjectsHelperOrderReduction({ tags, links }: Props) {
  return (
    <div>
      {tags.map(tag => (
        <span key={tag} className='tag'>
          {tag}
        </span>
      ))}
      {links.map(link => (
        <a href={link.url} key={link.label}>
          {link.label}
        </a>
      ))}
    </div>
  );
}
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Reduced from the website projects chunk.
// Reproduces exact output drift driven by helper ordering around adjacent
// JSX callback helpers.
interface Props {
  tags: string[];
  links: Array<{
    label: string;
    url: string;
  }>;
}
export default function ProjectsHelperOrderReduction(t0) {
  const $ = _c(7);
  const {
    tags,
    links
  } = t0;
  let t1;
  if ($[0] !== tags) {
    t1 = tags.map(_temp);
    $[0] = tags;
    $[1] = t1;
  } else {
    t1 = $[1];
  }
  let t2;
  if ($[2] !== links) {
    t2 = links.map(_temp2);
    $[2] = links;
    $[3] = t2;
  } else {
    t2 = $[3];
  }
  let t3;
  if ($[4] !== t1 || $[5] !== t2) {
    t3 = <div>{t1}{t2}</div>;
    $[4] = t1;
    $[5] = t2;
    $[6] = t3;
  } else {
    t3 = $[6];
  }
  return t3;
}
function _temp2(link) {
  return <a href={link.url} key={link.label}>{link.label}</a>;
}
function _temp(tag) {
  return <span key={tag} className="tag">{tag}</span>;
}
```
