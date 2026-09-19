import { Document } from "@langchain/core/documents";
import type { DocumentCounts, ExtractionConfig, Keyword, Metadata, ProcessingWarning, Table } from "@xberg-io/xberg";

// Metadata fields (from Xberg Metadata) that carry JSON-friendly scalar/list values,
// mapped from the binding's camelCase keys to the snake_case output keys used by the
// Python adapter. Opaque nested fields (pages, format, imagePreprocessing, jsonSchema,
// error) are skipped because they are native objects, not plain data. ~keep
const METADATA_FIELDS: ReadonlyArray<readonly [string, keyof Metadata]> = [
  ["title", "title"],
  ["subject", "subject"],
  ["authors", "authors"],
  ["keywords", "keywords"],
  ["language", "language"],
  ["created_at", "createdAt"],
  ["modified_at", "modifiedAt"],
  ["created_by", "createdBy"],
  ["modified_by", "modifiedBy"],
  ["category", "category"],
  ["tags", "tags"],
  ["document_version", "documentVersion"],
  ["abstract_text", "abstractText"],
  ["output_format", "outputFormat"],
  ["ocr_used", "ocrUsed"],
  ["extraction_duration_ms", "extractionDurationMs"],
];

const CONTENT_SEPARATOR = "\n\n";

export interface ChunkMetadataShape {
  chunkIndex: number;
  totalChunks: number;
  headingPath?: string[];
  tokenCount?: number;
  firstPage?: number;
  lastPage?: number;
}

export interface ChunkShape {
  content: string;
  chunkType: unknown;
  metadata: ChunkMetadataShape;
}

export interface PageShape {
  pageNumber: number;
  content: string;
  tables?: Table[];
  isBlank?: boolean;
}

// Structural view of a binding ExtractedDocument. The binding types its own fields
// through unexported `Js*` aliases; this interface uses the clean exported types plus
// local chunk/page shapes so the mapper is fully typed and testable without the addon. ~keep
export interface ExtractedDoc {
  content?: string;
  mimeType?: string;
  metadata?: Metadata;
  tables?: Table[];
  counts?: DocumentCounts;
  detectedLanguages?: string[];
  chunks?: ChunkShape[];
  pages?: PageShape[];
  extractedKeywords?: Keyword[];
  qualityScore?: number;
  processingWarnings?: ProcessingWarning[];
}

export interface ResultEnvelope {
  errors?: Array<{ source: string; message: string }>;
  results?: ExtractedDoc[];
}

export interface SplitOptions {
  chunking: boolean;
  perPage: boolean;
}

export function isChunkingEnabled(config?: ExtractionConfig | null): boolean {
  return config?.chunking != null;
}

export function isPerPageEnabled(config?: ExtractionConfig | null): boolean {
  return Boolean(config?.pages?.extractPages);
}

export function flattenMetadata(metadata?: Metadata): Record<string, unknown> {
  const flat: Record<string, unknown> = {};
  if (!metadata) {
    return flat;
  }

  for (const [snakeKey, camelKey] of METADATA_FIELDS) {
    const value = metadata[camelKey];
    if (value != null) {
      flat[snakeKey] = value;
    }
  }

  const additional = metadata.additional;
  if (additional && Object.keys(additional).length > 0) {
    flat.additional = { ...additional };
  }

  return flat;
}

export function assembleContent(content: string | undefined, tables: Table[] | undefined): string {
  const text = content ?? "";
  if (!tables || tables.length === 0) {
    return text;
  }
  const parts = tables.map((table) => table.markdown ?? "").filter((markdown) => markdown.length > 0);
  if (parts.length === 0) {
    return text;
  }
  return [text, ...parts].join(CONTENT_SEPARATOR);
}

export function buildMetadata(document: ExtractedDoc, source: string): Record<string, unknown> {
  const metadata = flattenMetadata(document.metadata);

  metadata.mime_type = document.mimeType;
  if (document.qualityScore != null) {
    metadata.quality_score = document.qualityScore;
  }
  if (document.detectedLanguages && document.detectedLanguages.length > 0) {
    metadata.detected_languages = document.detectedLanguages;
  }
  if (document.counts != null) {
    metadata.page_count = document.counts.pages;
  }

  if (document.extractedKeywords && document.extractedKeywords.length > 0) {
    metadata.extracted_keywords = document.extractedKeywords.map((keyword) => ({
      text: keyword.text,
      score: keyword.score,
      algorithm: String(keyword.algorithm),
    }));
  }

  const tables = document.tables ?? [];
  metadata.table_count = tables.length;
  if (tables.length > 0) {
    metadata.tables = tables.map((table) => ({
      cells: table.cells,
      markdown: table.markdown,
      page_number: table.pageNumber,
    }));
  }

  if (document.processingWarnings && document.processingWarnings.length > 0) {
    metadata.processing_warnings = document.processingWarnings.map((warning) => ({
      source: warning.source,
      message: warning.message,
    }));
  }

  metadata.source = source;
  return metadata;
}

function chunksToDocuments(document: ExtractedDoc, source: string): Document[] {
  const baseMetadata = buildMetadata(document, source);
  const documents: Document[] = [];

  for (const chunk of document.chunks ?? []) {
    const metadata: Record<string, unknown> = { ...baseMetadata };
    const chunkMetadata = chunk.metadata;
    metadata.chunk_index = chunkMetadata.chunkIndex;
    metadata.total_chunks = chunkMetadata.totalChunks;
    metadata.chunk_type = String(chunk.chunkType);
    if (chunkMetadata.headingPath && chunkMetadata.headingPath.length > 0) {
      metadata.heading_path = [...chunkMetadata.headingPath];
    }
    if (chunkMetadata.tokenCount != null) {
      metadata.token_count = chunkMetadata.tokenCount;
    }
    if (chunkMetadata.firstPage != null) {
      // Xberg uses 1-indexed pages; LangChain convention is 0-indexed. ~keep
      metadata.page = chunkMetadata.firstPage - 1;
      metadata.first_page = chunkMetadata.firstPage;
    }
    if (chunkMetadata.lastPage != null) {
      metadata.last_page = chunkMetadata.lastPage;
    }
    documents.push(new Document({ pageContent: chunk.content, metadata }));
  }

  return documents;
}

function pagesToDocuments(document: ExtractedDoc, source: string): Document[] {
  const baseMetadata = buildMetadata(document, source);
  const documents: Document[] = [];

  for (const page of document.pages ?? []) {
    // Xberg uses 1-indexed pages; LangChain convention is 0-indexed. ~keep
    const metadata: Record<string, unknown> = { ...baseMetadata, page: page.pageNumber - 1 };
    if (page.isBlank != null) {
      metadata.is_blank = page.isBlank;
    }
    const pageContent = assembleContent(page.content, page.tables);
    documents.push(new Document({ pageContent, metadata }));
  }

  return documents;
}

export function documentToDocuments(document: ExtractedDoc, source: string, options: SplitOptions): Document[] {
  if (options.chunking && document.chunks && document.chunks.length > 0) {
    return chunksToDocuments(document, source);
  }
  if (options.perPage && document.pages && document.pages.length > 0) {
    return pagesToDocuments(document, source);
  }
  const metadata = buildMetadata(document, source);
  const pageContent = assembleContent(document.content, document.tables);
  return [new Document({ pageContent, metadata })];
}

export function resultToDocuments(result: ResultEnvelope, sources: string[], options: SplitOptions): Document[] {
  if (result.errors && result.errors.length > 0) {
    const error = result.errors[0];
    throw new Error(`Failed to extract '${error.source}': ${error.message}`);
  }

  const documents: Document[] = [];
  const results = result.results ?? [];
  results.forEach((document, index) => {
    const source = sources[index] ?? (sources.length > 0 ? sources[sources.length - 1] : "");
    documents.push(...documentToDocuments(document, source, options));
  });
  return documents;
}
