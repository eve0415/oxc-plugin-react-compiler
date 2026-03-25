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
