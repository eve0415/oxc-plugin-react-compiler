// Reduced from /tmp/website/src/routes/projects/index.tsx
// Real-world drift: Babel and OXC emit different hoisted temp-function order
// for adjacent JSX callback helpers even when the rendered structure is the
// same.
function Component({ tags, links }) {
  return (
    <div>
      {tags.map(tag => (
        <span key={tag} className="tag">
          {tag}
        </span>
      ))}
      {links.map(link => (
        <a key={link.label} href={link.url}>
          {link.label}
        </a>
      ))}
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [
    {
      tags: ['react', 'rust'],
      links: [
        { label: 'docs', url: '/docs' },
        { label: 'repo', url: '/repo' },
      ],
    },
  ],
};
