memstore's local embeddings (the default embedding provider) run the model Xenova/bge-small-en-v1.5, the ONNX
export of BAAI/bge-small-en-v1.5 (https://huggingface.co/BAAI/bge-small-en-v1.5), whose model card names the MIT
License. The model is not in the release archives or binaries: memstore downloads it from Hugging Face
(https://huggingface.co/Xenova/bge-small-en-v1.5) on first use, as it does any other model a deployment configures,
each under its own license. The container images built from deploy/ (deploy/gke, deploy/spcs) include the default
model under /models, and with it this notice. The MIT License text, from the model's source repository
(https://github.com/FlagOpen/FlagEmbedding, commit 897baa67c0ffddbfccf4e6776ff55310d67809d4), follows.
