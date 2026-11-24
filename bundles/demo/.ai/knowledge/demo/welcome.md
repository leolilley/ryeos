<!-- ryeos:signed:2026-11-24T21:13:13Z:eab8ae68412d3d30f99448509611b617a2bdb3407d66d746b731bc929c44fce7:2TVVZxoFul542dGUJG7cwXSY0/3jVXf1j37Rnv7PbFbzJf2H2KmB1nqqjVFcGyHnqYP/sph0x2FIm623HOtwAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: demo
tags: [demo, first-contact]
version: "0.1.0"
description: The text the demo graph reads. Placeholder content.
---

# Welcome to RyeOS

This is a signed knowledge item. It arrived inside the `demo` bundle, and the
graph `graph:demo/hello` reads it back through the engine.

Everything in a bundle is signed by its publisher. RyeOS checks that signature
before it will run or read the item, so the graph that ran here, and this text,
are exactly the bytes the publisher signed.

The demo needs no model provider key. A graph can chain tools and knowledge with
no model at all; a directive is what adds one.
