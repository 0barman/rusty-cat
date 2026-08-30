# Provider feature flags: direct vs presigned/SAS

This guide explains what these four provider feature flags mean and how to choose between them:

- `aliyun-oss-direct`
- `azure-blob-direct`
- `aliyun-oss-presigned`
- `azure-blob-sas`

## Short version

`aliyun-oss-direct` and `azure-blob-direct` are direct credential features. The process running `rusty-cat` holds cloud credentials and the SDK signs requests before sending them to the storage provider.

`aliyun-oss-presigned` and `azure-blob-sas` are backend-signed URL features. A trusted backend keeps the cloud credentials and returns temporary URLs or tokens. The client process uses those URLs to upload/download chunks, while `rusty-cat` handles chunk I/O, retry, progress, and task lifecycle.

Use direct features for trusted backend services, internal CLIs, or controlled workers. Use presigned/SAS features for desktop, mobile, browser, or other client-side applications where long-lived cloud credentials should not be shipped to the user.

## Feature map

| Feature | Provider | Credential model | Main public types/helpers | Additional enabled items |
|---|---|---|---|---|
| `aliyun-oss-direct` | Aliyun OSS | The `rusty-cat` process receives Aliyun AccessKey credentials and signs OSS requests with OSS Signature Version 4. | `AliOssDirectUpload`, `AliOssDirectDownload` | `hmac`, `sha2`, `time` |
| `azure-blob-direct` | Azure Blob Storage | The `rusty-cat` process receives the Azure Storage account name/key and signs requests with Shared Key authentication. | `AzureBlobDirectUpload`, `AzureBlobDirectDownload` | `base64`, `hmac`, `sha2`, `time` |
| `aliyun-oss-presigned` | Aliyun OSS | A trusted backend creates short-lived OSS presigned URLs. The SDK does not generate Aliyun signatures in this feature. | `AliOssPresignedMultipartUpload`, `AliOssPresignedRangeDownload`, provider-neutral presigned primitives | `presigned` |
| `azure-blob-sas` | Azure Blob Storage | A trusted backend creates SAS URLs/tokens. The SDK does not generate Azure Shared Key signatures or Entra ID tokens in this feature. | `AzureBlobSasMultipartUpload`, `AzureBlobSasRangeDownload`, block/SAS helper functions, provider-neutral presigned primitives | `presigned`, `base64` |

The crate has no default provider features. Enable only the provider and credential model your application needs.

## Direct features

Direct mode means the SDK talks to the cloud provider directly and performs request signing locally.

Choose a direct feature when:

- The process is trusted to hold cloud credentials.
- The runtime is a backend service, internal command-line tool, private worker, or other controlled environment.
- You want fewer backend transfer endpoints because the process can sign upload/download requests itself.

Direct mode is usually not appropriate for public client applications. If an AccessKey secret or storage account key is embedded in a desktop, mobile, or browser client, a user or attacker may extract it. The blast radius depends on the permissions attached to that credential.

Provider details:

- `aliyun-oss-direct` implements Aliyun OSS multipart upload and range download with OSS Signature Version 4 signing.
- `azure-blob-direct` implements Azure block blob upload and range download with Shared Key signing.

## Presigned/SAS features

Presigned/SAS mode means a trusted backend authorizes each transfer and returns scoped, temporary URLs to the client.

Choose a presigned/SAS feature when:

- The transfer runs in an untrusted or partially trusted client process.
- Your backend must decide which user may access which object.
- You want short-lived, scoped permissions instead of shipping long-lived cloud secrets.
- You can support URL generation, refresh, and upload completion from your backend.

In this mode `rusty-cat` does not hold provider credentials. It executes the transfer plan supplied by your application: part URLs for upload, range URLs for download, optional completion requests, optional known object sizes, and optional refresh metadata.

Provider naming differs slightly:

- Aliyun commonly uses the term presigned URL, so the feature is `aliyun-oss-presigned`.
- Azure uses Shared Access Signature, so the feature is `azure-blob-sas`.

These two features follow the same security idea: credentials stay on your backend, while the client receives temporary permission to perform a specific transfer.

## Which feature should I enable?

| Scenario | Recommended feature |
|---|---|
| Backend service uploads/downloads OSS objects and can securely load AccessKey credentials. | `aliyun-oss-direct` |
| Backend service uploads/downloads Azure blobs and can securely load the storage account key. | `azure-blob-direct` |
| Public client uploads/downloads OSS objects through backend-issued temporary URLs. | `aliyun-oss-presigned` |
| Public client uploads/downloads Azure blobs through backend-issued SAS URLs. | `azure-blob-sas` |
| You want the safer default for user-facing applications. | `aliyun-oss-presigned` or `azure-blob-sas` |
| You are running test-app scenarios or broad integration tests and want every provider API available. | `all` |

## Runnable test-app coverage

The repository's runnable transfer checks live in
[`test-app`](../../test-app/README.md). Aliyun presigned range download has a
dedicated scenario:

```text
cargo run --manifest-path test-app/Cargo.toml -- aliyun-presigned
```

Backend-issued OSS/Azure upload URLs and Azure SAS download coverage run through
the `loonadm` scenario:

```text
cargo run --manifest-path test-app/Cargo.toml -- loonadm
```

The direct scenarios use only the official SDK provider implementations; the
former hand-written signers were not migrated. Credentials are read only from
the launching process's environment.

For Aliyun OSS, set `RC_ALIYUN_BUCKET`, `RC_ALIYUN_ACCESS_KEY_ID`, and
`RC_ALIYUN_ACCESS_KEY_SECRET`. Optional settings are `RC_ALIYUN_REGION`,
`RC_ALIYUN_OBJECT_PREFIX`, `RC_DIRECT_UPLOAD_SIZE`, `RC_DIRECT_PART_SIZE`, and
`RC_OUT_DIR`:

```text
cargo run --manifest-path test-app/Cargo.toml -- aliyun-direct
```

For Azure Blob, set `RC_AZURE_ACCOUNT_NAME`, `RC_AZURE_ACCOUNT_KEY`, and
`RC_AZURE_CONTAINER`. Optional settings are `RC_AZURE_BLOB_PREFIX`,
`RC_DIRECT_UPLOAD_SIZE`, `RC_DIRECT_PART_SIZE`, and `RC_OUT_DIR`:

```text
cargo run --manifest-path test-app/Cargo.toml -- azure-direct
```

## Related features and aliases

The crate also defines a few related convenience features and aliases:

| Feature | Meaning |
|---|---|
| `presigned` | Enables provider-neutral presigned multipart upload and range download primitives. It is pulled in by `aliyun-oss-presigned` and `azure-blob-sas`. |
| `aliyun-oss` | Alias for `aliyun-oss-presigned`. |
| `azure-blob` | Alias for `azure-blob-sas`. |
| `oss-providers` | Enables `aliyun-oss` and `azure-blob`, which currently means the presigned/SAS provider flows. |
| `all` | Enables all four provider features: both direct and backend-signed URL flows. |

The `aliyun-oss` and `azure-blob` aliases intentionally point to the backend-signed URL variants because those are usually the safer default for user-facing integrations.

## Individual guides

- [Aliyun OSS direct upload/download](aliyun-oss-direct.md)
- [Aliyun OSS presigned upload/download](aliyun-oss-presigned.md)
- [Azure Blob direct upload/download](azure-blob-direct.md)
- [Azure Blob SAS upload/download](azure-blob-sas.md)
