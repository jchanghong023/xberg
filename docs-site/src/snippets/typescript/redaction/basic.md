```typescript title="TypeScript"
import { ExtractInputKind, RedactionStrategy, extract } from '@xberg-io/xberg';

const output = await extract({
    kind: ExtractInputKind.Uri,
    uri: "contract.pdf",
}, {
    redaction: {
        categories: [
            { type: 'email' },
            { type: 'phone' },
            { type: 'ssn' },
            { type: 'credit_card' },
            { type: 'iban' },
        ],
        strategy: RedactionStrategy.Mask,
    },
});
const [result] = output.results ?? [];
console.log(result?.content);
console.log(`Redacted ${result?.redactionReport?.totalRedacted ?? 0} spans`);
```
