```go title="Go"
package main

import (
	"fmt"
	"log"

	"github.com/xberg-io/xberg/packages/go"
)

func main() {
	mode := "moderate"

	cfg := xberg.ExtractionConfig{
		// TokenReductionOptions only exposes Mode and PreserveImportantWords; Markdown
		// preservation is controlled by the reduction level's own defaults, not a separate flag.
		TokenReduction: &xberg.TokenReductionOptions{
			Mode: &mode,
		},
	}

	input := xberg.ExtractInputFromURI("verbose_document.pdf")
	result, err := xberg.Extract(*input, cfg)
	if err != nil {
		log.Fatalf("extraction failed: %v", err)
	}

	fmt.Printf("Reduced content:\n%s\n", result.Results[0].Content)
}
```
