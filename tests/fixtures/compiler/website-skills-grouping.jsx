// Reduced from /tmp/website/src/routes/skills/index.tsx
// Real-world drift: OXC reuses loop destructuring temporaries and emits a
// different helper ordering than Babel in the grouped skills rendering path.
import { useState } from 'react';

function SkillCard({ skill, index }) {
  return <div data-index={index}>{skill.name}</div>;
}

function Component({ skills, aiSkills }) {
  const [selectedSkillName, setSelectedSkillName] = useState();

  const groupedSkills = {};
  for (const skill of skills) {
    const { category } = skill;
    const arr = groupedSkills[category];
    if (arr) arr.push(skill);
    else groupedSkills[category] = [skill];
  }

  const aiDiscoveredSkills = aiSkills.filter(s => s.is_ai_discovered);
  const groupedAISkills = {};
  for (const skill of aiDiscoveredSkills) {
    const { category } = skill;
    const arr = groupedAISkills[category];
    if (arr) arr.push(skill);
    else groupedAISkills[category] = [skill];
  }

  return (
    <div>
      {Object.keys(groupedSkills).map(category => (
        <section key={category}>
          {groupedSkills[category]?.map((skill, index) => (
            <SkillCard key={skill.name} skill={skill} index={index} />
          ))}
          {groupedAISkills[category]?.map((aiSkill, index) => (
            <button
              type="button"
              key={aiSkill.name}
              onClick={() => {
                setSelectedSkillName(aiSkill.name);
              }}
            >
              {index + 1}
            </button>
          ))}
        </section>
      ))}
      <span>{selectedSkillName}</span>
    </div>
  );
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [
    {
      skills: [
        { name: 'React', category: 'frontend' },
        { name: 'TypeScript', category: 'frontend' },
        { name: 'Rust', category: 'systems' },
      ],
      aiSkills: [
        { name: 'Compiler', category: 'systems', is_ai_discovered: true },
        { name: 'Testing', category: 'frontend', is_ai_discovered: true },
      ],
    },
  ],
};
