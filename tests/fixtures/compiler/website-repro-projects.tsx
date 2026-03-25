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
