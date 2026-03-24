## Input

```javascript
// Reduced from the website skills visualization path.
// Reproduces exact transform drift plus a real AST mismatch involving
// callback state updates, canvas handlers, and effect-local draw setup.
import { useCallback, useEffect, useRef, useState } from 'react';

interface Skill {
  name: string;
  category: string;
}

interface AISkill extends Skill {
  is_ai_discovered: boolean;
  phase: string;
  progress: number;
}

function Tracker(props: {
  skill: AISkill;
  index: number;
  onStateChange: (index: number, phase: string, progress: number) => void;
}) {
  useEffect(() => {
    props.onStateChange(props.index, props.skill.phase, props.skill.progress);
  }, [props.index, props.onStateChange, props.skill.phase, props.skill.progress]);

  return null;
}

interface Props {
  skills: Skill[];
  aiSkills: AISkill[];
  selectedSkillId: string | null;
  onNodeSelect(name: string | null): void;
}

export default function SkillsVisualizationReduction({ skills, aiSkills, selectedSkillId, onNodeSelect }: Props) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const nodesRef = useRef<
    Array<{ x: number; y: number; radius: number; label: string; category: string; phase?: string; progress?: number }>
  >([]);
  const [dimensions, setDimensions] = useState({ width: 0, height: 0 });
  const [hoveredNode, setHoveredNode] = useState<string | null>(null);
  const [aiMaterializeStates, setAIMaterializeStates] = useState(new Map<number, { phase: string; progress: number }>());

  const handleMaterializeChange = useCallback((index: number, phase: string, progress: number) => {
    setAIMaterializeStates(prev => {
      const next = new Map(prev);
      next.set(index, { phase, progress });
      return next;
    });
  }, []);

  useEffect(() => {
    if (dimensions.width === 0 || dimensions.height === 0) return;

    const { width, height } = dimensions;
    const radiusBase = Math.min(width, height) * 0.35;
    const centerX = width / 2;
    const centerY = height / 2;
    const allNodes: Array<{
      x: number;
      y: number;
      radius: number;
      label: string;
      category: string;
      phase?: string;
      progress?: number;
    }> = [];

    for (const [index, skill] of skills.entries()) {
      const angle = (index / skills.length) * Math.PI * 2;
      const radius = radiusBase * 0.8;
      allNodes.push({
        x: centerX + Math.cos(angle) * radius,
        y: centerY + Math.sin(angle) * radius,
        radius: 4,
        label: skill.name,
        category: skill.category,
      });
    }

    const aiOnlySkills = aiSkills.filter(skill => skill.is_ai_discovered);
    for (const [index, aiSkill] of aiOnlySkills.entries()) {
      const angle = (index / Math.max(aiOnlySkills.length, 1)) * Math.PI * 2 + Math.PI / 4;
      const materializeState = aiMaterializeStates.get(index) ?? { phase: 'hidden', progress: 0 };
      allNodes.push({
        x: centerX + Math.cos(angle) * radiusBase,
        y: centerY + Math.sin(angle) * radiusBase,
        radius: 7,
        label: aiSkill.name,
        category: aiSkill.category,
        phase: materializeState.phase,
        progress: materializeState.progress,
      });
    }

    nodesRef.current = allNodes;
  }, [aiMaterializeStates, aiSkills, dimensions, skills]);

  const handleCanvasClick = useCallback(
    (event: React.MouseEvent<HTMLCanvasElement>) => {
      const canvas = canvasRef.current;
      if (!canvas) return;

      const rect = canvas.getBoundingClientRect();
      const x = event.clientX - rect.left;
      const y = event.clientY - rect.top;

      for (const node of nodesRef.current) {
        const dx = node.x - x;
        const dy = node.y - y;
        if (Math.hypot(dx, dy) <= node.radius + 5) {
          onNodeSelect(node.label);
          return;
        }
      }

      onNodeSelect(null);
    },
    [onNodeSelect],
  );

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    let animationId = 0;
    let time = 0;

    const setupCanvas = () => {
      const width = canvas.offsetWidth;
      const height = canvas.offsetHeight;
      return { width, height };
    };

    const draw = () => {
      const { width, height } = setupCanvas();
      ctx.clearRect(0, 0, width, height);
      time += 0.01;

      for (const node of nodesRef.current) {
        const isSelected = selectedSkillId === node.label;
        if (isSelected && hoveredNode) {
          ctx.fillText(node.label, node.x, node.y);
        }
      }

      animationId = requestAnimationFrame(draw);
    };

    const { width, height } = setupCanvas();
    setDimensions({ width, height });
    draw();

    return () => {
      cancelAnimationFrame(animationId);
    };
  }, [hoveredNode, selectedSkillId]);

  const className = `size-full ${hoveredNode ? 'cursor-pointer' : ''}`;

  return (
    <div>
      <canvas
        className={className}
        onClick={handleCanvasClick}
        onMouseMove={event => {
          const canvas = canvasRef.current;
          if (!canvas) return;

          const rect = canvas.getBoundingClientRect();
          const x = event.clientX - rect.left;
          const y = event.clientY - rect.top;

          let found: string | null = null;
          for (const node of nodesRef.current) {
            const dx = node.x - x;
            const dy = node.y - y;
            if (Math.hypot(dx, dy) <= node.radius + 5) {
              found = node.label;
              break;
            }
          }

          setHoveredNode(found);
        }}
        ref={canvasRef}
      />
      {aiSkills.filter(skill => skill.is_ai_discovered).map((skill, index) => (
        <Tracker index={index} key={skill.name} onStateChange={handleMaterializeChange} skill={skill} />
      ))}
    </div>
  );
}
```

## Code

```javascript
import { c as _c } from "react/compiler-runtime";
// Reduced from the website skills visualization path.
// Reproduces exact transform drift plus a real AST mismatch involving
// callback state updates, canvas handlers, and effect-local draw setup.
import { useCallback, useEffect, useRef, useState } from 'react';
interface Skill {
  name: string;
  category: string;
}
interface AISkill extends Skill {
  is_ai_discovered: boolean;
  phase: string;
  progress: number;
}
function Tracker(props) {
  const $ = _c(7);
  let t0;
  if ($[0] !== props) {
    t0 = () => {
      props.onStateChange(props.index, props.skill.phase, props.skill.progress);
    };
    $[0] = props;
    $[1] = t0;
  } else {
    t0 = $[1];
  }
  let t1;
  if ($[2] !== props.index || $[3] !== props.onStateChange || $[4] !== props.skill.phase || $[5] !== props.skill.progress) {
    t1 = [props.index, props.onStateChange, props.skill.phase, props.skill.progress];
    $[2] = props.index;
    $[3] = props.onStateChange;
    $[4] = props.skill.phase;
    $[5] = props.skill.progress;
    $[6] = t1;
  } else {
    t1 = $[6];
  }
  useEffect(t0, t1);
  return null;
}
interface Props {
  skills: Skill[];
  aiSkills: AISkill[];
  selectedSkillId: string | null;
  onNodeSelect(name: string | null): void;
}
export default function SkillsVisualizationReduction(t0) {
  const $ = _c(26);
  const {
    skills,
    aiSkills,
    selectedSkillId,
    onNodeSelect
  } = t0;
  const canvasRef = useRef(null);
  let t1;
  if ($[0] === Symbol.for("react.memo_cache_sentinel")) {
    t1 = [];
    $[0] = t1;
  } else {
    t1 = $[0];
  }
  const nodesRef = useRef(t1);
  let t2;
  if ($[1] === Symbol.for("react.memo_cache_sentinel")) {
    t2 = {
      width: 0,
      height: 0
    };
    $[1] = t2;
  } else {
    t2 = $[1];
  }
  const [dimensions, setDimensions] = useState(t2);
  const [hoveredNode, setHoveredNode] = useState(null);
  let t3;
  if ($[2] === Symbol.for("react.memo_cache_sentinel")) {
    t3 = new Map();
    $[2] = t3;
  } else {
    t3 = $[2];
  }
  const [aiMaterializeStates, setAIMaterializeStates] = useState(t3);
  let t4;
  if ($[3] === Symbol.for("react.memo_cache_sentinel")) {
    t4 = (index, phase, progress) => {
      setAIMaterializeStates(prev => {
        const next = new Map(prev);
        next.set(index, {
          phase,
          progress
        });
        return next;
      });
    };
    $[3] = t4;
  } else {
    t4 = $[3];
  }
  const handleMaterializeChange = t4;
  let t5;
  let t6;
  if ($[4] !== aiMaterializeStates || $[5] !== aiSkills || $[6] !== dimensions || $[7] !== skills) {
    t5 = () => {
      if (dimensions.width === 0 || dimensions.height === 0) {
        return;
      }
      const {
        width,
        height
      } = dimensions;
      const radiusBase = Math.min(width, height) * 0.35;
      const centerX = width / 2;
      const centerY = height / 2;
      const allNodes = [];
      for (const [index_0, skill] of skills.entries()) {
        const angle = index_0 / skills.length * Math.PI * 2;
        const radius = radiusBase * 0.8;
        allNodes.push({
          x: centerX + Math.cos(angle) * radius,
          y: centerY + Math.sin(angle) * radius,
          radius: 4,
          label: skill.name,
          category: skill.category
        });
      }
      const aiOnlySkills = aiSkills.filter(_temp);
      for (const [index_1, aiSkill] of aiOnlySkills.entries()) {
        const angle_0 = index_1 / Math.max(aiOnlySkills.length, 1) * Math.PI * 2 + Math.PI / 4;
        const materializeState = aiMaterializeStates.get(index_1) ?? {
          phase: "hidden",
          progress: 0
        };
        allNodes.push({
          x: centerX + Math.cos(angle_0) * radiusBase,
          y: centerY + Math.sin(angle_0) * radiusBase,
          radius: 7,
          label: aiSkill.name,
          category: aiSkill.category,
          phase: materializeState.phase,
          progress: materializeState.progress
        });
      }
      nodesRef.current = allNodes;
    };
    t6 = [aiMaterializeStates, aiSkills, dimensions, skills];
    $[4] = aiMaterializeStates;
    $[5] = aiSkills;
    $[6] = dimensions;
    $[7] = skills;
    $[8] = t5;
    $[9] = t6;
  } else {
    t5 = $[8];
    t6 = $[9];
  }
  useEffect(t5, t6);
  let t7;
  if ($[10] !== onNodeSelect) {
    t7 = event => {
      const canvas = canvasRef.current;
      if (!canvas) {
        return;
      }
      const rect = canvas.getBoundingClientRect();
      const x = event.clientX - rect.left;
      const y = event.clientY - rect.top;
      for (const node of nodesRef.current) {
        const dx = node.x - x;
        const dy = node.y - y;
        if (Math.hypot(dx, dy) <= node.radius + 5) {
          onNodeSelect(node.label);
          return;
        }
      }
      onNodeSelect(null);
    };
    $[10] = onNodeSelect;
    $[11] = t7;
  } else {
    t7 = $[11];
  }
  const handleCanvasClick = t7;
  let t8;
  let t9;
  if ($[12] !== hoveredNode || $[13] !== selectedSkillId) {
    t8 = () => {
      const canvas_0 = canvasRef.current;
      if (!canvas_0) {
        return;
      }
      const ctx = canvas_0.getContext("2d");
      if (!ctx) {
        return;
      }
      let animationId = 0;
      let time = 0;
      const setupCanvas = () => {
        const width_0 = canvas_0.offsetWidth;
        const height_0 = canvas_0.offsetHeight;
        return {
          width: width_0,
          height: height_0
        };
      };
      const draw = () => {
        const {
          width: width_1,
          height: height_1
        } = setupCanvas();
        ctx.clearRect(0, 0, width_1, height_1);
        time = time + 0.01;
        time;
        for (const node_0 of nodesRef.current) {
          const isSelected = selectedSkillId === node_0.label;
          if (isSelected && hoveredNode) {
            ctx.fillText(node_0.label, node_0.x, node_0.y);
          }
        }
        animationId = requestAnimationFrame(draw);
      };
      const {
        width: width_2,
        height: height_2
      } = setupCanvas();
      setDimensions({
        width: width_2,
        height: height_2
      });
      draw();
      return () => {
        cancelAnimationFrame(animationId);
      };
    };
    t9 = [hoveredNode, selectedSkillId];
    $[12] = hoveredNode;
    $[13] = selectedSkillId;
    $[14] = t8;
    $[15] = t9;
  } else {
    t8 = $[14];
    t9 = $[15];
  }
  useEffect(t8, t9);
  const className = `size-full ${hoveredNode ? "cursor-pointer" : ""}`;
  let t10;
  if ($[16] === Symbol.for("react.memo_cache_sentinel")) {
    t10 = event_0 => {
      const canvas_1 = canvasRef.current;
      if (!canvas_1) {
        return;
      }
      const rect_0 = canvas_1.getBoundingClientRect();
      const x_0 = event_0.clientX - rect_0.left;
      const y_0 = event_0.clientY - rect_0.top;
      let found = null;
      for (const node_1 of nodesRef.current) {
        const dx_0 = node_1.x - x_0;
        const dy_0 = node_1.y - y_0;
        if (Math.hypot(dx_0, dy_0) <= node_1.radius + 5) {
          found = node_1.label;
          break;
        }
      }
      setHoveredNode(found);
    };
    $[16] = t10;
  } else {
    t10 = $[16];
  }
  let t11;
  if ($[17] !== className || $[18] !== handleCanvasClick) {
    t11 = <canvas className={className} onClick={handleCanvasClick} onMouseMove={t10} ref={canvasRef} />;
    $[17] = className;
    $[18] = handleCanvasClick;
    $[19] = t11;
  } else {
    t11 = $[19];
  }
  let t12;
  if ($[20] !== aiSkills) {
    let t13;
    if ($[22] === Symbol.for("react.memo_cache_sentinel")) {
      t13 = (skill_2, index_2) => <Tracker index={index_2} key={skill_2.name} onStateChange={handleMaterializeChange} skill={skill_2} />;
      $[22] = t13;
    } else {
      t13 = $[22];
    }
    t12 = aiSkills.filter(_temp2).map(t13);
    $[20] = aiSkills;
    $[21] = t12;
  } else {
    t12 = $[21];
  }
  let t13;
  if ($[23] !== t11 || $[24] !== t12) {
    t13 = <div>{t11}{t12}</div>;
    $[23] = t11;
    $[24] = t12;
    $[25] = t13;
  } else {
    t13 = $[25];
  }
  return t13;
}
function _temp2(skill_1) {
  return skill_1.is_ai_discovered;
}
function _temp(skill_0) {
  return skill_0.is_ai_discovered;
}
```
