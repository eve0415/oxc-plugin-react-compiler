export interface TransformOptions {
  compilationMode?: 'infer' | 'annotation' | 'all';
  panicThreshold?: 'none' | 'all';
  target?: string;
}

export interface TransformResult {
  transformed: boolean;
  code: string;
  map: string | null;
}

export function transform(
  filename: string,
  source: string,
  options?: TransformOptions,
): TransformResult;
