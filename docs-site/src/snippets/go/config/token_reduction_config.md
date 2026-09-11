```go title="Go"
package main

import (
	"fmt"

	"github.com/xberg-io/xberg/packages/go"
)

func main() {
	preserveImportant := true
	config := xberg.ExtractionConfig{
		TokenReduction: &xberg.TokenReductionOptions{
			Mode:                   xberg.Ptr("moderate"),
			PreserveImportantWords: &preserveImportant,
		},
	}

	fmt.Printf("Mode: %s, Preserve Important Words: %v\n",
		config.TokenReduction.Mode,
		*config.TokenReduction.PreserveImportantWords)
}
```
