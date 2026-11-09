export declare const DEFAULT_URL: string;
export declare const TOKEN_KINDS: string[];
export declare const KINDS: CaptchaKind[];

export type CaptchaKind = "image" | "slider" | "gap" | "detect" | "audio" | "math";
export type Source = string | Uint8Array | ArrayBuffer | ArrayBufferView | Blob | Response;

export interface SolveOptions {
  /** image: digits, lower, upper, letters, alnum, lower_digits, upper_digits, or literal characters. */
  charset?: string;
  case?: "lower" | "upper" | "any";
  /** Expected answer length; the result then carries `length_ok`. */
  length?: number;
  /** Force the kind for a bare image source (default image). */
  type?: CaptchaKind;
  /** For an HTML Response: base URL to resolve the captcha <img src> against. */
  baseUrl?: string;
  /** For an HTML Response: how to fetch the captcha image (pass your cookie-carrying fetch). */
  fetchImage?: typeof fetch;
  imageInit?: RequestInit;
}

export interface CaptchaRequest extends SolveOptions {
  type?: CaptchaKind;
  kind?: CaptchaKind;
  image?: Source;
  background?: Source;
  audio?: Source;
  question?: string;
}

export interface CaptchaAnswer {
  v: 1;
  ok: boolean;
  kind: CaptchaKind;
  /** image / audio / math */
  text?: string;
  confidence?: number | null;
  length_ok?: boolean;
  /** slider / gap: centre of the target */
  x?: number;
  y?: number;
  /** detect */
  boxes?: [number, number, number, number][];
  /** audio: what whisper heard */
  transcript?: string;
  /** HTML input: the captcha image that was solved */
  src?: string;
  engine?: string;
  job_id?: string;
  worker_peer_id?: string;
  duration_ms?: number;
}

export interface SolverOptions {
  url?: string;
  token?: string;
  fetch?: typeof fetch;
  timeoutMs?: number;
}

export interface Solver {
  url: string;
  solve(input: Source | CaptchaRequest, options?: SolveOptions): Promise<CaptchaAnswer>;
  image(image: Source, options?: SolveOptions): Promise<CaptchaAnswer>;
  slider(piece: Source, background: Source): Promise<CaptchaAnswer>;
  gap(withGap: Source, clean: Source): Promise<CaptchaAnswer>;
  detect(image: Source): Promise<CaptchaAnswer>;
  audio(audio: Source, options?: SolveOptions): Promise<CaptchaAnswer>;
  math(question: string): Promise<CaptchaAnswer>;
}

export declare class UnsupportedCaptchaError extends Error {
  kind: string;
  siteKey?: string;
}
export declare class CaptchaError extends Error {
  status: number;
}

export declare function createSolver(options?: SolverOptions): Solver;
export declare function solveCaptcha(input: Source | CaptchaRequest, options?: SolveOptions): Promise<CaptchaAnswer>;
export declare function solveWith(options: SolveOptions): (input: Source | CaptchaRequest) => Promise<CaptchaAnswer>;
export declare function findCaptcha(
  html: string,
  baseUrl?: string,
): { kind: "image"; src: string } | { kind: "recaptcha" | "hcaptcha" | "turnstile"; siteKey?: string } | null;
export declare function sniff(bytes: Uint8Array | ArrayBuffer | ArrayBufferView): "image" | "audio" | null;
export declare function looksLikeQuestion(s: string): boolean;
export default solveCaptcha;
