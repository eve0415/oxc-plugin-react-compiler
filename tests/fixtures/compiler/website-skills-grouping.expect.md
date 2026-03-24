## Input

```javascript
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
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Reduced from /tmp/website/src/routes/skills/index.tsx
// Real-world drift: OXC reuses loop destructuring temporaries and emits a
// different helper ordering than Babel in the grouped skills rendering path.
import { useState } from 'react';
function SkillCard(t0) {
  const $ = _c(3);
  const {
    skill,
    index
  } = t0;
  let t1;
  if ($[0] !== index || $[1] !== skill.name) {
    t1 = <div data-index={index}>{skill.name}</div>;
    $[0] = index;
    $[1] = skill.name;
    $[2] = t1;
  } else {
    t1 = $[2];
  }
  return t1;
}
function Component(t0) {
  const $ = _c(8);
  const {
    skills,
    aiSkills
  } = t0;
  const [selectedSkillName, setSelectedSkillName] = useState();
  let t1;
  if ($[0] !== aiSkills || $[1] !== skills) {
    const groupedSkills = {};
    for (const skill of skills) {
      const {
        category
      } = skill;
      const arr = groupedSkills[category];
      if (arr) {
        arr.push(skill);
      } else {
        groupedSkills[category] = [skill];
      }
    }
    const aiDiscoveredSkills = aiSkills.filter(_temp);
    const groupedAISkills = {};
    for (const skill_0 of aiDiscoveredSkills) {
      const {
        category: category_0
      } = skill_0;
      const arr_0 = groupedAISkills[category_0];
      if (arr_0) {
        arr_0.push(skill_0);
      } else {
        groupedAISkills[category_0] = [skill_0];
      }
    }
    t1 = Object.keys(groupedSkills).map(category_1 => <section key={category_1}>{groupedSkills[category_1]?.map(_temp2)}{groupedAISkills[category_1]?.map((aiSkill, index_0) => <button type="button" key={aiSkill.name} onClick={() => {
        setSelectedSkillName(aiSkill.name);
      }}>{index_0 + 1}</button>)}</section>);
    $[0] = aiSkills;
    $[1] = skills;
    $[2] = t1;
  } else {
    t1 = $[2];
  }
  let t2;
  if ($[3] !== selectedSkillName) {
    t2 = <span>{selectedSkillName}</span>;
    $[3] = selectedSkillName;
    $[4] = t2;
  } else {
    t2 = $[4];
  }
  let t3;
  if ($[5] !== t1 || $[6] !== t2) {
    t3 = <div>{t1}{t2}</div>;
    $[5] = t1;
    $[6] = t2;
    $[7] = t3;
  } else {
    t3 = $[7];
  }
  return t3;
}
function _temp2(skill_1, index) {
  return <SkillCard key={skill_1.name} skill={skill_1} index={index} />;
}
function _temp(s) {
  return s.is_ai_discovered;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: [{
    skills: [{
      name: 'React',
      category: 'frontend'
    }, {
      name: 'TypeScript',
      category: 'frontend'
    }, {
      name: 'Rust',
      category: 'systems'
    }],
    aiSkills: [{
      name: 'Compiler',
      category: 'systems',
      is_ai_discovered: true
    }, {
      name: 'Testing',
      category: 'frontend',
      is_ai_discovered: true
    }]
  }]
};
```
