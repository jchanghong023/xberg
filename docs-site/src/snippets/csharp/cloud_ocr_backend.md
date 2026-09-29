```csharp title="C#"
using Xberg;
using System;
using System.Collections.Generic;
using System.Net.Http;
using System.Text.Json;
using System.Threading.Tasks;

public class CloudOcrBackend : IOcrBackend
{
    private readonly string _apiKey;
    private readonly List<string> _langs = new() { "eng", "deu", "fra" };
    private readonly HttpClient _httpClient = new();

    public CloudOcrBackend(string apiKey)
    {
        _apiKey = apiKey;
    }

    public string Name => "cloud-ocr";
    public string Version => "1.0.0";
    public OcrBackendType BackendType => OcrBackendType.Custom;
    public List<string> SupportedLanguages => _langs;
    public bool SupportsTableDetection => false;
    public bool SupportsDocumentProcessing => false;
    public bool EmitsStructuredMarkdown => false;
    // The cloud service reports no calibrated page confidence and needs an upright
    // raster, so declare the least-capable option for both descriptors.
    public ConfidenceSemantics ConfidenceSemantics => Xberg.ConfidenceSemantics.Uncalibrated;
    public PageOrientationHandling PageOrientationHandling => Xberg.PageOrientationHandling.RequiresUpright;

    public void Initialize() { }
    public void Shutdown() => _httpClient.Dispose();

    public bool SupportsLanguage(string lang) => _langs.Contains(lang);

    // The cloud service offers the same languages whatever the extraction config asks for, so
    // both config-aware members delegate to the config-independent set above. A backend whose
    // language support depends on the config (a model chosen per request, say) would read it here.
    public List<string> SupportedLanguagesFor(OcrConfig config) => _langs;

    public bool SupportsLanguageFor(OcrConfig config, string language) => _langs.Contains(language);

    public ExtractedDocument ProcessImage(byte[] imageBytes, OcrConfig config)
    {
        using var form = new MultipartFormDataContent();
        form.Add(new ByteArrayContent(imageBytes), "image");
        var lang = config.Language.Count > 0 ? config.Language[0] : "eng";
        form.Add(new StringContent(lang), "language");

        var response = _httpClient.PostAsync("https://api.example.com/ocr", form).Result;
        var json = response.Content.ReadAsStringAsync().Result;
        var doc = JsonDocument.Parse(json);
        var text = doc.RootElement.GetProperty("text").GetString() ?? "";

        return new ExtractedDocument
        {
            Content = text,
            MimeType = "text/plain",
            Metadata = new Metadata(),
        };
    }

    public ExtractedDocument ProcessImageFile(string path, OcrConfig config) =>
        ProcessImage(System.IO.File.ReadAllBytes(path), config);

    public ExtractedDocument ProcessDocument(string path, OcrConfig config) =>
        throw new OcrException("cloud-ocr does not support whole-document processing");
}

class Program
{
    static void Main()
    {
        var backend = new CloudOcrBackend(apiKey: "your-api-key");
        OcrBackendRegistry.RegisterOcrBackend(backend);
    }
}
```
