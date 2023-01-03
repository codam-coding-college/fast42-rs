# Fast42 - Rust Edition
A super fast 42 API connector

Makes it easy to fetch data from the 42 API.
Main features:
- Rate Limited
- Async (easily fetch all pages of an endpoint!)
- Fast 🚀

## Running examples
- Initialize your `secrets.yaml` file in the `examples` folder using the template from `secrets_example.yaml`
- Run the example like this: `cargo run --example users`

# TODO

- [x] add patch/put/post/delete
- [x] add scopes
- [ ] add multi key support
- [ ] implement doing requests with user access token
- [x] solve potential issues with duplicate http options (`page[size]`)
- [ ] publish to crates.io
