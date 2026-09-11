"""Tests for DocumentPipeline."""

from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from surrealdb.errors import ServerError
from xberg import (
    Chunk,
    ChunkMetadata,
    ChunkType,
    EmbeddingModelType,
    ExtractedDocument,
    ExtractionResult,
)

from tests.conftest import EMBEDDING_DIMENSIONS, make_chunk, make_document
from surrealdb_xberg._base import _check_insert_result
from surrealdb_xberg.exceptions import DimensionMismatchError, IngestionError, SchemaNotInitializedError
from surrealdb_xberg.pipeline import DocumentPipeline

_INPUT = "surrealdb_xberg._base._input_from_path"
_EXTRACT = "surrealdb_xberg._base.extract"
_PIPELINE_EXTRACT = "surrealdb_xberg.pipeline.extract"


def _single_result(document: ExtractedDocument) -> ExtractionResult:
    return ExtractionResult(results=[document])


def _probe_result(dimensions: int) -> ExtractionResult:
    chunk = make_chunk(0, embedding=[0.1] * dimensions)
    return ExtractionResult(results=[make_document(chunks=[chunk])])


def test_pipeline_defaults(mock_client: AsyncMock) -> None:
    pipeline = DocumentPipeline(db=mock_client)
    assert pipeline._embed is True
    assert pipeline.chunk_table == "chunks"
    # Dimensions are unknown until probed at setup (or supplied explicitly). ~keep
    assert pipeline.embedding_dimensions is None


def test_pipeline_explicit_dimensions(mock_client: AsyncMock) -> None:
    pipeline = DocumentPipeline(db=mock_client, embedding_dimensions=1024)
    assert pipeline.embedding_dimensions == 1024


def test_pipeline_embedding_model_type_direct(mock_client: AsyncMock) -> None:
    model = EmbeddingModelType.preset("balanced")
    pipeline = DocumentPipeline(db=mock_client, embedding_model=model, embedding_dimensions=768)
    assert pipeline.embedding_dimensions == 768


def test_pipeline_embed_false(mock_client: AsyncMock) -> None:
    pipeline = DocumentPipeline(db=mock_client, embed=False)
    assert pipeline._embed is False


def test_pipeline_custom_chunk_table(mock_client: AsyncMock) -> None:
    pipeline = DocumentPipeline(db=mock_client, chunk_table="my_chunks")
    assert pipeline.chunk_table == "my_chunks"


def test_pipeline_extraction_config_has_chunking(mock_client: AsyncMock) -> None:
    pipeline = DocumentPipeline(db=mock_client)
    assert pipeline._config is not None
    assert pipeline._config.chunking is not None


def test_pipeline_embed_true_has_embedding(mock_client: AsyncMock) -> None:
    pipeline = DocumentPipeline(db=mock_client, embed=True)
    assert pipeline._config is not None
    assert pipeline._config.chunking.embedding is not None


def test_pipeline_embed_false_no_embedding(mock_client: AsyncMock) -> None:
    pipeline = DocumentPipeline(db=mock_client, embed=False)
    assert pipeline._config is not None
    assert pipeline._config.chunking.embedding is None


def test_pipeline_user_extraction_config_gets_chunking(mock_client: AsyncMock) -> None:
    from xberg import ExtractionConfig

    user_config = ExtractionConfig()
    pipeline = DocumentPipeline(db=mock_client, config=user_config)

    # ExtractionConfig is a frozen dataclass; a new config is returned and the
    # user's original object is left untouched. ~keep
    assert pipeline._config is not user_config
    assert user_config.chunking is None
    assert pipeline._config.chunking is not None
    assert pipeline._config.chunking.embedding is not None


def test_pipeline_preserves_user_chunking_params(mock_client: AsyncMock) -> None:
    from xberg import ChunkingConfig, ExtractionConfig

    user_config = ExtractionConfig(
        chunking=ChunkingConfig(max_characters=512, overlap=100),
    )
    pipeline = DocumentPipeline(db=mock_client, config=user_config)

    assert pipeline._config is not user_config
    assert user_config.chunking.embedding is None
    assert pipeline._config.chunking.max_characters == 512
    assert pipeline._config.chunking.overlap == 100
    assert pipeline._config.chunking.embedding is not None


def test_pipeline_preserves_user_chunking_params_embed_false(mock_client: AsyncMock) -> None:
    from xberg import ChunkingConfig, ExtractionConfig

    user_config = ExtractionConfig(
        chunking=ChunkingConfig(max_characters=256),
    )
    pipeline = DocumentPipeline(db=mock_client, config=user_config, embed=False)

    assert pipeline._config is not None
    assert pipeline._config.chunking is not None
    assert pipeline._config.chunking.max_characters == 256
    assert pipeline._config.chunking.embedding is None


@patch(_PIPELINE_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_setup_schema_probes_dimensions(
    mock_extract: AsyncMock,
    mock_client: AsyncMock,
) -> None:
    """When no dimensions are supplied, setup_schema probes them by embedding a string."""
    mock_extract.return_value = _probe_result(384)
    pipeline = DocumentPipeline(db=mock_client)

    await pipeline.setup_schema()

    mock_extract.assert_awaited_once()
    assert pipeline.embedding_dimensions == 384
    hnsw = [c.args[0] for c in mock_client.query.call_args_list if "idx_chunk_embedding" in c.args[0]]
    assert hnsw and "HNSW DIMENSION 384" in hnsw[0]


@patch(_PIPELINE_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_setup_schema_skips_probe_when_dimensions_given(
    mock_extract: AsyncMock,
    mock_client: AsyncMock,
) -> None:
    pipeline = DocumentPipeline(db=mock_client, embedding_dimensions=768)

    await pipeline.setup_schema()

    mock_extract.assert_not_awaited()
    assert pipeline.embedding_dimensions == 768


@patch(_PIPELINE_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_probe_raises_when_no_embedding(
    mock_extract: AsyncMock,
    mock_client: AsyncMock,
) -> None:
    mock_extract.return_value = ExtractionResult(results=[make_document(chunks=[])])
    pipeline = DocumentPipeline(db=mock_client)

    with pytest.raises(RuntimeError, match="Could not determine embedding dimensions"):
        await pipeline.setup_schema()


@patch(_PIPELINE_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_setup_schema_no_probe_when_embed_false(
    mock_extract: AsyncMock,
    mock_client: AsyncMock,
) -> None:
    pipeline = DocumentPipeline(db=mock_client, embed=False)

    await pipeline.setup_schema()

    mock_extract.assert_not_awaited()


@patch("surrealdb_xberg.pipeline.build_pipeline_schema")
async def test_pipeline_setup_schema_forwards_params(
    mock_build: MagicMock,
    mock_client: AsyncMock,
) -> None:
    """setup_schema() passes constructor and method parameters to build_pipeline_schema."""
    mock_build.return_value = ["STMT1;", "STMT2;", "STMT3;"]
    pipeline = DocumentPipeline(db=mock_client, chunk_table="my_chunks", embedding_dimensions=384)

    await pipeline.setup_schema(
        analyzer_language="german",
        bm25_k1=1.5,
        bm25_b=0.8,
        distance_metric="EUCLIDEAN",
        hnsw_efc=200,
        hnsw_m=16,
    )

    mock_build.assert_called_once_with(
        table="documents",
        chunk_table="my_chunks",
        embed=True,
        embedding_dimension=384,
        analyzer_language="german",
        bm25_k1=1.5,
        bm25_b=0.8,
        distance_metric="EUCLIDEAN",
        hnsw_efc=200,
        hnsw_m=16,
    )
    assert mock_client.query.call_count == 3


@patch(_INPUT, new_callable=AsyncMock)
@patch(_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_chunk_metadata_mapped(
    mock_extract: AsyncMock,
    _mock_input: AsyncMock,
    pipeline: DocumentPipeline,
    mock_client: AsyncMock,
) -> None:
    """Chunk rows map ChunkMetadata attributes: token_count and byte offsets."""
    chunk = make_chunk(2)
    mock_extract.return_value = _single_result(make_document(chunks=[chunk]))

    await pipeline.ingest_file("/tmp/test.pdf")

    chunk_call = mock_client.query.call_args_list[1]
    rec = chunk_call[0][1]["records"][0]
    assert rec["chunk_index"] == 2
    assert rec["word_count"] == chunk.metadata.token_count
    assert rec["char_start"] == chunk.metadata.byte_start
    assert rec["char_end"] == chunk.metadata.byte_end
    assert rec["page_number"] == chunk.metadata.first_page
    assert rec["first_page"] == chunk.metadata.first_page
    assert rec["last_page"] == chunk.metadata.last_page


@patch(_INPUT, new_callable=AsyncMock)
@patch(_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_chunk_without_page_metadata(
    mock_extract: AsyncMock,
    _mock_input: AsyncMock,
    pipeline: DocumentPipeline,
    mock_client: AsyncMock,
) -> None:
    """Chunks whose page fields are None still map cleanly."""
    meta = ChunkMetadata(
        byte_start=0,
        byte_end=50,
        chunk_index=0,
        total_chunks=1,
        heading_path=[],
        image_indices=[],
        node_ids=[],
        page_spans=[],
        classifications=[],
        token_count=None,
        first_page=None,
        last_page=None,
    )
    chunk = Chunk(content="No page info.", chunk_type=ChunkType.UNKNOWN, metadata=meta, embedding=[0.1] * 768)
    mock_extract.return_value = _single_result(make_document(chunks=[chunk]))

    await pipeline.ingest_file("/tmp/test.pdf")

    rec = mock_client.query.call_args_list[1][0][1]["records"][0]
    assert rec["page_number"] is None
    assert rec["first_page"] is None
    assert rec["last_page"] is None
    assert rec["word_count"] is None
    assert rec["char_start"] == 0
    assert rec["char_end"] == 50
    assert rec["content"] == "No page info."
    assert rec["chunk_index"] == 0


@patch(_INPUT, new_callable=AsyncMock)
@patch(_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_ingest_file_no_chunks_skips_chunk_insert(
    mock_extract: AsyncMock,
    _mock_input: AsyncMock,
    pipeline: DocumentPipeline,
    mock_client: AsyncMock,
) -> None:
    mock_extract.return_value = _single_result(make_document(chunks=[]))

    await pipeline.ingest_file("/tmp/test.pdf")

    assert mock_client.query.call_count == 1
    assert "INSERT IGNORE INTO documents" in mock_client.query.call_args[0][0]


@patch(_INPUT, new_callable=AsyncMock)
@patch(_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_chunk_batch_splitting(
    mock_extract: AsyncMock,
    _mock_input: AsyncMock,
    mock_client: AsyncMock,
    sample_chunks: list[Chunk],
) -> None:
    """With insert_batch_size=2 and 3 chunks, chunks split into 2 INSERT queries."""
    mock_extract.return_value = _single_result(make_document(chunks=sample_chunks))

    pipeline = DocumentPipeline(db=mock_client, insert_batch_size=2, embedding_dimensions=EMBEDDING_DIMENSIONS)
    await pipeline.setup_schema()
    mock_client.query.reset_mock()

    await pipeline.ingest_file("/tmp/test.pdf")

    # 1 document insert + 2 chunk-batch inserts. ~keep
    assert mock_client.query.call_count == 3
    assert len(mock_client.query.call_args_list[1][0][1]["records"]) == 2
    assert len(mock_client.query.call_args_list[2][0][1]["records"]) == 1


@patch(_INPUT, new_callable=AsyncMock)
@patch("surrealdb_xberg._base.extract_batch", new_callable=AsyncMock)
async def test_pipeline_ingest_files_batches_docs(
    mock_extract_batch: AsyncMock,
    _mock_input: AsyncMock,
    pipeline: DocumentPipeline,
    mock_client: AsyncMock,
) -> None:
    """ingest_files() uses one extract_batch and one batched document insert."""
    documents = [make_document(content=f"doc {i}", chunks=[make_chunk(0)]) for i in range(3)]
    mock_extract_batch.return_value = ExtractionResult(results=documents)

    await pipeline.ingest_files(["/a.pdf", "/b.pdf", "/c.pdf"])

    mock_extract_batch.assert_awaited_once()
    # First query is the single batched document insert of all three rows. ~keep
    doc_call = mock_client.query.call_args_list[0]
    assert "INSERT IGNORE INTO documents" in doc_call[0][0]
    assert len(doc_call[0][1]["records"]) == 3


def test_check_insert_result_passes_on_normal_results() -> None:
    _check_insert_result([], context="test")
    _check_insert_result([{"id": "rec:1"}], context="test")
    _check_insert_result(None, context="test")


def test_check_insert_result_raises_on_dimension_error() -> None:
    result = ["Expected a vector of 768 dimensions, but got 384"]
    with pytest.raises(DimensionMismatchError, match="Vector dimension mismatch"):
        _check_insert_result(result, context="chunk insertion")


def test_check_insert_result_raises_on_generic_string_error() -> None:
    result = ["Some unexpected SurrealDB error"]
    with pytest.raises(IngestionError, match="INSERT IGNORE failed silently"):
        _check_insert_result(result, context="test")


@patch(_INPUT, new_callable=AsyncMock)
@patch(_EXTRACT, new_callable=AsyncMock)
async def test_pipeline_raises_on_chunk_dimension_mismatch(
    mock_extract: AsyncMock,
    _mock_input: AsyncMock,
    pipeline: DocumentPipeline,
    mock_client: AsyncMock,
    sample_chunks: list[Chunk],
) -> None:
    """SurrealDB 2.0 raises ServerError on the chunk insert; it maps to DimensionMismatchError."""
    mock_extract.return_value = _single_result(make_document(chunks=sample_chunks))
    mock_client.query = AsyncMock(
        side_effect=[
            [],
            ServerError("Query", "Incorrect vector dimension (384). Expected 768."),
        ],
    )

    with pytest.raises(DimensionMismatchError, match="Vector dimension mismatch during chunk insertion"):
        await pipeline.ingest_file("/tmp/test.pdf")


@patch(_PIPELINE_EXTRACT, new_callable=AsyncMock)
async def test_embed_query_raises_on_empty_chunks(mock_extract: AsyncMock, mock_client: AsyncMock) -> None:
    mock_extract.return_value = ExtractionResult(results=[make_document(chunks=[])])
    pipeline = DocumentPipeline(db=mock_client, embed=True)

    with pytest.raises(RuntimeError, match="Embedding generation failed"):
        await pipeline.embed_query("test query")


@patch(_PIPELINE_EXTRACT, new_callable=AsyncMock)
async def test_embed_query_raises_on_none_embedding(mock_extract: AsyncMock, mock_client: AsyncMock) -> None:
    meta = ChunkMetadata(
        byte_start=0,
        byte_end=1,
        chunk_index=0,
        total_chunks=1,
        heading_path=[],
        image_indices=[],
        node_ids=[],
        page_spans=[],
        classifications=[],
    )
    chunk = Chunk(content="x", chunk_type=ChunkType.UNKNOWN, metadata=meta, embedding=None)
    mock_extract.return_value = ExtractionResult(results=[make_document(chunks=[chunk])])
    pipeline = DocumentPipeline(db=mock_client, embed=True)

    with pytest.raises(RuntimeError, match="Embedding generation failed"):
        await pipeline.embed_query("test query")


@patch(_PIPELINE_EXTRACT, new_callable=AsyncMock)
async def test_embed_query_returns_embedding(mock_extract: AsyncMock, mock_client: AsyncMock) -> None:
    """embed_query() returns the embedding vector from the first chunk."""
    expected = [0.1, 0.2, 0.3]
    chunk = make_chunk(0, embedding=expected)
    mock_extract.return_value = ExtractionResult(results=[make_document(chunks=[chunk])])
    pipeline = DocumentPipeline(db=mock_client, embed=True)

    result = await pipeline.embed_query("test query")

    # Embeddings round-trip through f32, so compare approximately. ~keep
    assert result == pytest.approx(expected)
    mock_extract.assert_awaited_once()


@pytest.mark.parametrize(
    ("method", "args", "kwargs"),
    [
        ("ingest_file", ["/tmp/test.pdf"], {}),
        ("ingest_files", [["/tmp/a.pdf", "/tmp/b.pdf"]], {}),
        ("ingest_directory", ["/tmp"], {}),
        ("ingest_bytes", [], {"data": b"hello", "mime_type": "text/plain", "source": "test"}),
    ],
    ids=["ingest_file", "ingest_files", "ingest_directory", "ingest_bytes"],
)
async def test_pipeline_raises_without_schema(
    mock_client: AsyncMock,
    method: str,
    args: list[object],
    kwargs: dict[str, object],
) -> None:
    pipeline = DocumentPipeline(db=mock_client, embed=False)

    with pytest.raises(SchemaNotInitializedError, match="setup_schema"):
        await getattr(pipeline, method)(*args, **kwargs)
