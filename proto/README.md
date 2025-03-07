# Protobuf
We use protobuf mainly to communicate with substreams. You can use the [language support extension](https://marketplace.visualstudio.com/items?itemName=pbkit.vscode-pbkit) and for formatting use the [buf extension for VS Code](https://marketplace.visualstudio.com/items?itemName=bufbuild.vscode-buf).
1. To communicate with substreams, we always want to use their latest protobuf images. Usually the sf module is already committed but in case there was an update we can pull the latest version into the repo with:
```
buf export buf.build/streamingfast/substreams -o ./proto
```
2. Now that we have all our protobuf modules together we can generate the respective Rust code for them:
```
buf generate 
```

## Protobuf Linting
1. To install the linter execute:
```
brew install bufbuild/buf/buf@1.28.1
```
2. Linting the files works like:
```
buf lint
```
It will return a list of errors that you need to fix. If not, then all the changes you made conform to the linter.