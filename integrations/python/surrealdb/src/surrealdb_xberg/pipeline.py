"""Chunked extraction with optional embedding for RAG pipelines."""

from dataclasses import replace
from typing import TYPE_CHECKING

from surrealdb import RecordID
from xberg import (
    ChunkingConfig,
    EmbeddingConfig,
    EmbeddingModelType,
    ExtractedDocument,
    ExtractInput,
    ExtractionConfig,
    extract,
)

from surrealdb_xberg._base import (
    AsyncSurrealQueryable,
    BaseIngester,
    _content_hash,
    _execute_insert,
    _map_result_to_doc,
)
from surrealdb_xberg.schema import build_pipeline_schema

if TYPE_CHECKING:
    from surrealdb_xberg.types import ChunkRecord

_PROBE_TEXT = b"embedding dimension probe"


class DocumentPipeline(BaseIngester):
    """Chunked extraction with optional embedding for RAG pipelines."""

    def __init__(
        self,
        *,
        db: AsyncSurrealQueryable,
        table: str = "documents",
        insert_batch_size: int = 100,
        chunk_table: str = "chunks",
        config: ExtractionConfig | None = None,
        embed: bool = True,
        embedding_model: str | EmbeddingModelType = "balanced",
        embedding_dimensions: int | None = None,
    ) -> None:
        """Initialize the pipeline.

        Args:
            db: An active SurrealDB async connection.
            table: Name of the documents table.
            insert_batch_size: Max records per INSERT IGNORE batch.
            chunk_table: Name of the chunks table.
            config: Optional Xberg ExtractionConfig. If it includes a
                ChunkingConfig, the chunking parameters are preserved and only
                the embedding config is injected.
            embed: Whether to generate embeddings for vector search.
            embedding_model: Preset name (e.g. ``"balanced"``, ``"fast"``) or
                an ``EmbeddingModelType`` instance.
            embedding_dimensions: Vector dimensions for the HNSW index. When
                omitted, the dimension is probed once during ``setup_schema`` by
                embedding a short string and reading ``len(chunk.embedding)``.
                Pass it explicitly to skip the probe.

        """
        super().__init__(db=db, table=table, config=config)
        self._insert_batch_size = insert_batch_size
        self._chunk_table = chunk_table
        self._embed = embed

        if isinstance(embedding_model, str):
            self._embedding_model_type: EmbeddingModelType = EmbeddingModelType.preset(embedding_model)
        else:
            self._embedding_model_type = embedding_model

        self._embedding_dimensions: int | None = embedding_dimensions
        self._config = self._build_extraction_config()

    @property
    def chunk_table(self) -> str:
        """The chunks table name."""
        return self._chunk_table

    @property
    def embedding_dimensions(self) -> int | None:
        """The vector embedding dimensions, or ``None`` until probed/provided."""
        return self._embedding_dimensions

    def _build_extraction_config(self) -> ExtractionConfig:
        """Build ExtractionConfig with chunking and optional embedding.

        ``ExtractionConfig`` and ``ChunkingConfig`` are frozen dataclasses. If
        the user provided a config with a ``ChunkingConfig``, its chunking
        parameters (``max_characters``, ``overlap``, ``preset``) are preserved
        and only the embedding configuration is replaced. Returns a new
        ``ExtractionConfig``; the caller's original object, if any, is not
        mutated.

        Returns:
            A fully configured ExtractionConfig with chunking (and optionally
            embedding) enabled.

        """
        embedding = EmbeddingConfig(model=self._embedding_model_type) if self._embed else None

        config = self._config if self._config is not None else ExtractionConfig()
        user_chunking = config.chunking
        if user_chunking is not None:
            chunking = replace(user_chunking, embedding=embedding)
        else:
            chunking = ChunkingConfig(embedding=embedding)
        return replace(config, chunking=chunking)

    async def _probe_embedding_dimensions(self) -> int:
        """Determine the embedding dimension by embedding a short probe string.

        Returns:
            The length of the embedding vector produced for the probe.

        Raises:
            RuntimeError: If Xberg returns no embedding for the probe.

        """
        result = await extract(ExtractInput(kind="bytes", bytes=_PROBE_TEXT, mime_type="text/plain"), self._config)
        document = result.results[0] if result.results else None
        chunk = document.chunks[0] if document is not None and document.chunks else None
        if chunk is None or chunk.embedding is None:
            msg = "Could not determine embedding dimensions: no embedding returned for probe text"
            raise RuntimeError(msg)
        return len(chunk.embedding)

    async def setup_schema(
        self,
        *,
        analyzer_language: str = "english",
        bm25_k1: float = 1.2,
        bm25_b: float = 0.75,
        distance_metric: str = "COSINE",
        hnsw_efc: int = 150,
        hnsw_m: int = 12,
    ) -> None:
        """Create documents + chunks tables with BM25 and HNSW indexes.

        When embeddings are enabled and no ``embedding_dimensions`` was supplied
        to the constructor, the vector dimension is probed once here before the
        HNSW index is defined.

        Args:
            analyzer_language: Snowball stemmer language for the BM25 analyzer.
            bm25_k1: BM25 term-frequency saturation parameter.
            bm25_b: BM25 document-length normalization parameter.
            distance_metric: HNSW distance function (e.g. ``"COSINE"``, ``"EUCLIDEAN"``).
            hnsw_efc: HNSW construction-time search width (higher = slower build, better recall).
            hnsw_m: HNSW max edges per node (higher = more memory, better recall).

        """
        if self._embed and self._embedding_dimensions is None:
            self._embedding_dimensions = await self._probe_embedding_dimensions()

        stmts = build_pipeline_schema(
            table=self._table,
            chunk_table=self._chunk_table,
            embed=self._embed,
            embedding_dimension=self._embedding_dimensions or 0,
            analyzer_language=analyzer_language,
            bm25_k1=bm25_k1,
            bm25_b=bm25_b,
            distance_metric=distance_metric,
            hnsw_efc=hnsw_efc,
            hnsw_m=hnsw_m,
        )
        for stmt in stmts:
            await self._client.query(stmt)
        self._schema_ready = True

    def _build_chunk_records(
        self,
        document: ExtractedDocument,
        doc_id: RecordID,
        content_hash: str,
    ) -> "list[ChunkRecord]":
        """Build chunk rows for a single document from its ``Chunk`` objects.

        Args:
            document: The extracted document carrying the chunks.
            doc_id: The parent document's ``RecordID`` for the record link.
            content_hash: The parent document's content hash for deterministic chunk IDs.

        Returns:
            A list of chunk records ready for insertion.

        """
        records: list[ChunkRecord] = []
        for index, chunk in enumerate(document.chunks or []):
            meta = chunk.metadata
            chunk_rec: ChunkRecord = {
                "id": RecordID(self._chunk_table, f"{content_hash}_{index}"),
                "document": doc_id,
                "content": chunk.content,
                "chunk_index": meta.chunk_index,
                "embedding": chunk.embedding if self._embed else None,
                "word_count": meta.token_count,
                "page_number": meta.first_page,
                "char_start": meta.byte_start,
                "char_end": meta.byte_end,
                "first_page": meta.first_page,
                "last_page": meta.last_page,
            }
            records.append(chunk_rec)
        return records

    async def _insert_batched(self, table: str, rows: "list[ChunkRecord] | list[dict]", *, context: str) -> None:
        """Insert rows into ``table`` in ``insert_batch_size`` chunks.

        Args:
            table: Destination table name.
            rows: The record rows to insert.
            context: A human-readable label for error messages.

        """
        for start in range(0, len(rows), self._insert_batch_size):
            batch = rows[start : start + self._insert_batch_size]
            await _execute_insert(
                self._client,
                f"INSERT IGNORE INTO {table} $records",
                list(batch),
                context=context,
            )

    async def _ingest_batch(self, documents: list[tuple[ExtractedDocument, str]]) -> None:
        """Store documents then their chunks, both batched and idempotent.

        Document rows are collected and inserted together; chunk rows across all
        documents are collected and inserted in ``insert_batch_size`` batches.
        Deterministic record IDs plus ``INSERT IGNORE`` keep the pipeline
        idempotent and resilient to partial failures.

        Args:
            documents: ``(document, source)`` pairs to persist.

        """
        if not documents:
            return

        doc_rows: list[dict] = []
        chunk_rows: list[ChunkRecord] = []
        for document, source in documents:
            content_hash = _content_hash(document.content)
            doc = _map_result_to_doc(document, source, self._table)
            doc_rows.append(dict(doc))
            chunk_rows.extend(self._build_chunk_records(document, doc["id"], content_hash))

        await self._insert_batched(self._table, doc_rows, context="document insertion")
        if chunk_rows:
            await self._insert_batched(self._chunk_table, chunk_rows, context="chunk insertion")

    async def embed_query(self, query: str) -> list[float]:
        """Embed a query string using xberg's extraction pipeline.

        Args:
            query: The text to embed.

        Returns:
            The embedding vector as a list of floats.

        Raises:
            RuntimeError: If Xberg returns no embedding for the query.

        """
        result = await extract(
            ExtractInput(kind="bytes", bytes=query.encode(), mime_type="text/plain"),
            self._config,
        )
        document = result.results[0] if result.results else None
        chunk = document.chunks[0] if document is not None and document.chunks else None
        if chunk is None or chunk.embedding is None:
            msg = "Embedding generation failed: no embedding returned for query"
            raise RuntimeError(msg)
        embedding: list[float] = chunk.embedding
        return embedding
