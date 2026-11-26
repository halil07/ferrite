# JSONPlaceholder Studio E2E & Network Mocking Example

This example demonstrates Ferrite's hybrid network mocking (`mock.load` + `mock.when` with `:param` path substitution, `params`/`data` subset matching, `delay`, and pure response JSON fixtures) across all 11 routes of the **JSONPlaceholder Studio** demo app.

- **Demo Site Repository**: https://github.com/halil07/ferrite-mock-example-site

## Running the Example

1. Clone and start the demo site (`http://localhost:5173`):

```bash
git clone https://github.com/halil07/ferrite-mock-example-site.git
cd ferrite-mock-example-site
npm install
npm run dev
```

2. Validate and run the `.ferrite` test suites from the Ferrite repository root:

```bash
npx ferrite check examples/jsonplaceholder-studio
npx ferrite run examples/jsonplaceholder-studio
```
