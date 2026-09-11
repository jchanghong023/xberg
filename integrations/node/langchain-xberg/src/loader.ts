import { stat } from "node:fs/promises";
import { BaseDocumentLoader } from "@langchain/core/document_loaders/base";
import type { Document } from "@langchain/core/documents";
import { extract, extractBatch, ExtractInputKind } from "@xberg-io/xberg";
import type { ExtractInput, ExtractionConfig } from "@xberg-io/xberg";
import fastGlob from "fast-glob";
import { isChunkingEnabled, isPerPageEnabled, resultToDocuments, type ResultEnvelope } from "./mapping";

const DEFAULT_GLOB = "**/*";

export interface XbergLoaderOptions {
  /** File path, list of file paths, or directory path to load. */
  filePath?: string | string[];
  /** Raw bytes to extract from. Mutually exclusive with `filePath`. */
  data?: Uint8Array;
  /** MIME type hint. Required when using `data`, optional for `filePath`. */
  mimeType?: string;
  /** Glob pattern for directory mode. Defaults to matching all files. */
  glob?: string;
  /** Xberg extraction configuration controlling output format, OCR, pages, chunking, etc. */
  config?: ExtractionConfig;
}

/**
 * Load documents using Xberg, supporting 107 file formats with true async extraction.
 *
 * By default each source becomes one Document. Enable `chunking` on the
 * `ExtractionConfig` to emit one Document per chunk, or `pages` for one Document
 * per page. Multiple paths (a list or a directory glob) are extracted with a single
 * `extractBatch` call so concurrency happens Rust-side.
 */
export class XbergLoader extends BaseDocumentLoader {
  private readonly filePath?: string | string[];
  private readonly data?: Uint8Array;
  private readonly mimeType?: string;
  private readonly glob?: string;
  private readonly config?: ExtractionConfig;

  constructor(options: XbergLoaderOptions) {
    super();
    const { filePath, data, mimeType, glob, config } = options;
    if (filePath === undefined && data === undefined) {
      throw new Error("Either 'filePath' or 'data' must be provided.");
    }
    if (filePath !== undefined && data !== undefined) {
      throw new Error("Cannot specify both 'filePath' and 'data'. Use one or the other.");
    }
    if (data !== undefined && mimeType === undefined) {
      throw new Error("'mimeType' is required when using 'data'.");
    }

    this.filePath = filePath;
    this.data = data;
    this.mimeType = mimeType;
    this.glob = glob;
    this.config = config;
  }

  async load(): Promise<Document[]> {
    const { inputs, sources, batch } = await this.buildInputs();
    if (inputs.length === 0) {
      return [];
    }

    let result;
    try {
      result = batch ? await extractBatch(inputs, this.config ?? null) : await extract(inputs[0], this.config ?? null);
    } catch (error) {
      const source = sources[0] ?? "input";
      throw new Error(`Failed to extract '${source}': ${errorMessage(error)}`, { cause: error });
    }

    return resultToDocuments(result as unknown as ResultEnvelope, sources, {
      chunking: isChunkingEnabled(this.config),
      perPage: isPerPageEnabled(this.config),
    });
  }

  private async buildInputs(): Promise<{ inputs: ExtractInput[]; sources: string[]; batch: boolean }> {
    if (this.data !== undefined) {
      const source = `bytes://${this.mimeType}`;
      const input: ExtractInput = { kind: ExtractInputKind.Bytes, bytes: this.data, mimeType: this.mimeType };
      return { inputs: [input], sources: [source], batch: false };
    }

    const { paths, batch } = await this.resolvePaths();
    const inputs: ExtractInput[] = paths.map((path) => ({
      kind: ExtractInputKind.Uri,
      uri: path,
      mimeType: this.mimeType,
    }));
    return { inputs, sources: paths, batch };
  }

  private async resolvePaths(): Promise<{ paths: string[]; batch: boolean }> {
    const filePath = this.filePath;
    if (Array.isArray(filePath)) {
      return { paths: filePath, batch: true };
    }
    if (typeof filePath === "string") {
      if (await isDirectory(filePath)) {
        const pattern = this.glob ?? DEFAULT_GLOB;
        const matches = await fastGlob(pattern, { cwd: filePath, onlyFiles: true, absolute: true });
        return { paths: matches.sort(), batch: true };
      }
      return { paths: [filePath], batch: false };
    }
    return { paths: [], batch: false };
  }
}

async function isDirectory(path: string): Promise<boolean> {
  try {
    const stats = await stat(path);
    return stats.isDirectory();
  } catch {
    return false;
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
