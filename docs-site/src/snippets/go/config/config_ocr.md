```go title="Go"
package main

import "github.com/xberg-io/xberg/packages/go"

func main() {
	psm := int32(3)

	_ = xberg.ExtractionConfig{
		Ocr: &xberg.OcrConfig{
			Backend:  xberg.Ptr("tesseract"),
			Language: []string{"eng", "fra"},
			TesseractConfig: &xberg.TesseractConfig{
				Psm: &psm,
			},
		},
	}
}
```
