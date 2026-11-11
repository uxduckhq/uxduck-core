`desktop-app.asar` is a real archive made by `@electron/asar` 3.4.1's
`createPackage`, with these two files:

```json
{ "name": "@uxduck/desktop", "version": "0.2.0", "main": "dist/main.js" }
```

`dist/main.js` contains only `// UXDuck metadata regression fixture.` and a
newline. There are no third-party or personal contents. The fixture checks the
reader against Electron's archive writer rather than a matching homemade writer.
