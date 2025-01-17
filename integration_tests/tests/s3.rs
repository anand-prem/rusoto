#![cfg(feature = "s3")]
extern crate env_logger;
extern crate futures;
extern crate futures_fs;
extern crate http;
extern crate log;
extern crate reqwest;
extern crate rusoto_core;
extern crate rusoto_s3;
extern crate time;

use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::str;
use std::time::Duration;
use time::get_time;

use futures::{Future, Stream};
use futures_fs::FsPool;
use rusoto_core::credential::{AwsCredentials, DefaultCredentialsProvider};
use rusoto_core::{ProvideAwsCredentials, Region, RusotoError};
use rusoto_s3::util::{PreSignedRequest, PreSignedRequestOption};
use rusoto_s3::{
    CORSConfiguration, CORSRule, CompleteMultipartUploadRequest, CompletedMultipartUpload,
    CompletedPart, CopyObjectRequest, CreateBucketRequest, CreateMultipartUploadRequest,
    DeleteBucketRequest, DeleteObjectRequest, GetObjectError, GetObjectRequest, HeadObjectRequest,
    ListObjectsRequest, ListObjectsV2Request, PutBucketCorsRequest, PutObjectRequest, S3Client,
    StreamingBody, UploadPartCopyRequest, UploadPartRequest, S3,
};

struct TestS3Client {
    region: Region,
    s3: S3Client,
    bucket_name: String,
    // This flag signifies whether this bucket was already deleted as part of a test
    bucket_deleted: bool,
}

impl TestS3Client {
    // construct S3 testing client
    fn new(bucket_name: String) -> TestS3Client {
        let region = if let Ok(endpoint) = env::var("S3_ENDPOINT") {
            let region = Region::Custom {
                name: "us-east-1".to_owned(),
                endpoint: endpoint.to_owned(),
            };
            println!(
                "picked up non-standard endpoint {:?} from S3_ENDPOINT env. variable",
                region
            );
            region
        } else {
            Region::UsEast1
        };

        TestS3Client {
            region: region.to_owned(),
            s3: S3Client::new(region),
            bucket_name: bucket_name.to_owned(),
            bucket_deleted: false,
        }
    }

    fn create_test_bucket(&self, name: String) {
        let create_bucket_req = CreateBucketRequest {
            bucket: name.clone(),
            ..Default::default()
        };
        self.s3
            .create_bucket(create_bucket_req)
            .sync()
            .expect("Failed to create test bucket");
    }

    fn delete_object(&self, key: String) {
        let delete_object_req = DeleteObjectRequest {
            bucket: self.bucket_name.to_owned(),
            key: key.to_owned(),
            ..Default::default()
        };

        self.s3
            .delete_object(delete_object_req)
            .sync()
            .expect("Couldn't delete object");
    }

    fn put_test_object(&self, filename: String) {
        let contents: Vec<u8> = Vec::new();
        let put_request = PutObjectRequest {
            bucket: self.bucket_name.to_owned(),
            key: filename.to_owned(),
            body: Some(contents.into()),
            ..Default::default()
        };

        self.s3
            .put_object(put_request)
            .sync()
            .expect("Failed to put test object");
    }
}

impl Drop for TestS3Client {
    fn drop(&mut self) {
        if self.bucket_deleted {
            return;
        }
        let delete_bucket_req = DeleteBucketRequest {
            bucket: self.bucket_name.clone(),
            ..Default::default()
        };

        match self.s3.delete_bucket(delete_bucket_req).sync() {
            Ok(_) => println!("Deleted S3 bucket: {}", self.bucket_name),
            Err(e) => println!("Failed to delete S3 bucket: {}", e),
        }
    }
}

// NOTE: Do we want to use logging calls instead of printlns??
// inititializes logging
fn init_logging() {
    let _ = env_logger::try_init();
}

#[test]
// creates a bucket and test listing buckets and items in bucket
fn test_bucket_creation_deletion() {
    init_logging();

    let bucket_name = format!("s3-test-bucket-{}", get_time().sec);
    let mut test_client = TestS3Client::new(bucket_name.clone());

    let create_bucket_req = CreateBucketRequest {
        bucket: bucket_name.clone(),
        ..Default::default()
    };

    // first create a bucket
    let create_bucket_resp = test_client.s3.create_bucket(create_bucket_req).sync();
    assert!(create_bucket_resp.is_ok());
    println!(
        "Bucket {} created, resp: {:#?}",
        bucket_name.clone(),
        create_bucket_resp.unwrap()
    );

    // now lets check for our bucket and list items in the one we created
    let resp = test_client.s3.list_buckets().sync();
    assert!(resp.is_ok());

    let resp = resp.unwrap();
    let mut bucket_found = false;
    for bucket in resp.buckets.unwrap().iter() {
        if bucket.name == Some(bucket_name.clone()) {
            bucket_found = true;
            break;
        }
    }
    assert!(bucket_found);

    let list_obj_req = ListObjectsV2Request {
        bucket: bucket_name.to_owned(),
        start_after: Some("foo".to_owned()),
        ..Default::default()
    };
    let result = test_client.s3.list_objects_v2(list_obj_req).sync();
    assert!(result.is_ok());

    test_delete_bucket(&test_client.s3, &bucket_name);
    test_client.bucket_deleted = true;
}

#[test]
// test against normal files
fn test_puts_gets_deletes() {
    init_logging();

    let bucket_name = format!("test-bucket-{}-{}", "default".to_owned(), get_time().sec);
    let test_client = TestS3Client::new(bucket_name.clone());
    test_client.create_test_bucket(bucket_name.clone());

    // modify the bucket's CORS properties
    if cfg!(not(feature = "disable_minio_unsupported")) {
        // Minio support: CORS is not implemented by Minio
        test_put_bucket_cors(&test_client.s3, &test_client.bucket_name);
    }

    // file used for testing puts/gets
    let filename = format!("test_file_{}", get_time().sec);

    // test failure responses on empty bucket
    test_get_object_no_such_object(&test_client.s3, &test_client.bucket_name, &filename);

    // PUT an object via buffer (no_credentials is an arbitrary choice)
    test_put_object_with_filename(
        &test_client.s3,
        &test_client.bucket_name,
        &filename,
        &"tests/sample-data/no_credentials",
    );

    // HEAD the object that was PUT
    test_head_object(&test_client.s3, &test_client.bucket_name, &filename);

    // GET the object
    test_get_object(&test_client.s3, &test_client.bucket_name, &filename);
    test_get_object_range(&test_client.s3, &test_client.bucket_name, &filename);

    // add two objects to test the listing by paging
    for i in 1..3 {
        test_client.put_test_object(format!("test_object_{}", i));
    }

    // list items with paging using list object API v1
    list_items_in_bucket_paged_v1(&test_client.s3, &test_client.bucket_name);

    // list items with paging using list object API v2
    if cfg!(not(feature = "disable_ceph_unsupported")) {
        // Ceph support: this test depends on the list object v2 API which is not implemented by Ceph
        list_items_in_bucket_paged_v2(&test_client.s3, &test_client.bucket_name);
    }

    // copy the object to change its settings
    test_copy_object(&test_client.s3, &test_client.bucket_name, &filename);

    // delete object, will also allow drop() to remove the bucket
    test_delete_object(&test_client.s3, &test_client.bucket_name, &filename);

    // remove test objects used for pagination tests
    for i in 1..3 {
        test_client.delete_object(format!("test_object_{}", i));
    }
}

#[test]
// test against utf8 files
fn test_puts_gets_deletes_utf8() {
    init_logging();

    let bucket_name = format!("test-bucket-{}-{}", "utf-8".to_owned(), get_time().sec);
    let test_client = TestS3Client::new(bucket_name.clone());
    test_client.create_test_bucket(bucket_name.clone());

    let utf8_filename = format!("test[über]file@{}", get_time().sec);
    // UTF8 filenames
    test_put_object_with_filename(
        &test_client.s3,
        &test_client.bucket_name,
        &utf8_filename,
        &"tests/sample-data/no_credentials",
    );

    test_copy_object_utf8(&test_client.s3, &test_client.bucket_name, &utf8_filename);
    test_delete_object(&test_client.s3, &test_client.bucket_name, &utf8_filename);
}

#[test]
// test against binary files
fn test_puts_gets_deletes_binary() {
    init_logging();

    let bucket_name = format!("test-bucket-{}-{}", "binary".to_owned(), get_time().sec);
    let test_client = TestS3Client::new(bucket_name.clone());
    test_client.create_test_bucket(bucket_name.clone());

    let binary_filename = format!("test_file_b{}", get_time().sec);

    // Binary objects:
    test_put_object_with_filename(
        &test_client.s3,
        &test_client.bucket_name,
        &binary_filename,
        &"tests/sample-data/binary-file",
    );
    test_get_object(&test_client.s3, &test_client.bucket_name, &binary_filename);
    test_get_object_blocking_read(&test_client.s3, &test_client.bucket_name, &binary_filename);
    test_delete_object(&test_client.s3, &test_client.bucket_name, &binary_filename);
}

#[test]
// test metadata ops
fn test_puts_gets_deletes_metadata() {
    init_logging();

    let bucket_name = format!("test-bucket-{}-{}", "metadata".to_owned(), get_time().sec);
    let test_client = TestS3Client::new(bucket_name.clone());
    test_client.create_test_bucket(bucket_name.clone());

    let metadata_filename = format!("test_metadata_file_{}", get_time().sec);
    let mut metadata = HashMap::<String, String>::new();
    metadata.insert(
        "rusoto-metadata-some".to_string(),
        "some-test-value".to_string(),
    );
    metadata.insert("rusoto-metadata-none".to_string(), "".to_string());

    test_put_object_with_metadata(
        &test_client.s3,
        &test_client.bucket_name,
        &metadata_filename,
        &"tests/sample-data/no_credentials",
        &metadata,
    );

    test_head_object_with_metadata(
        &test_client.s3,
        &test_client.bucket_name,
        &metadata_filename,
        &metadata,
    );
    test_get_object_with_metadata(
        &test_client.s3,
        &test_client.bucket_name,
        &metadata_filename,
        &metadata,
    );
    test_delete_object(
        &test_client.s3,
        &test_client.bucket_name,
        &metadata_filename,
    );
}

#[test]
// test object ops using presigned urls
fn test_puts_gets_deletes_presigned_url() {
    init_logging();

    let bucket_name = format!("test-bucket-{}-{}", "presigned".to_owned(), get_time().sec);
    let test_client = TestS3Client::new(bucket_name.clone());
    test_client.create_test_bucket(bucket_name.clone());

    let filename = format!("test_file_{}_for_presigned", get_time().sec);
    // PUT an object for presigned url
    test_put_object_with_filename(
        &test_client.s3,
        &test_client.bucket_name,
        &filename,
        &"tests/sample-data/no_credentials",
    );

    let credentials = DefaultCredentialsProvider::new()
        .unwrap()
        .credentials()
        .wait()
        .unwrap();

    // generate a presigned url
    test_get_object_with_presigned_url(
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &filename,
    );
    test_get_object_with_expired_presigned_url(
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &filename,
    );
    test_put_object_with_presigned_url(
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &filename,
    );
    test_delete_object_with_presigned_url(
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &filename,
    );

    let utf8_filename = format!("test[über]file@{}_for_presigned", get_time().sec);
    // UTF8 filenames for presigned url
    test_put_object_with_filename(
        &test_client.s3,
        &test_client.bucket_name,
        &utf8_filename,
        &"tests/sample-data/no_credentials",
    );
    // generate a presigned url
    test_get_object_with_presigned_url(
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &utf8_filename,
    );
    test_get_object_with_expired_presigned_url(
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &utf8_filename,
    );
    test_put_object_with_presigned_url(
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &utf8_filename,
    );
    test_delete_object_with_presigned_url(
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &utf8_filename,
    );
}

#[test]
fn test_multipart_stream_uploads() {
    init_logging();

    let bucket_name = format!("test-bucket-{}-{}", "multipart".to_owned(), get_time().sec);
    let test_client = TestS3Client::new(bucket_name.clone());
    test_client.create_test_bucket(bucket_name.clone());

    let multipart_filename = format!("test_multipart_file_{}", get_time().sec);
    let credentials = DefaultCredentialsProvider::new()
        .unwrap()
        .credentials()
        .wait()
        .unwrap();

    // test put via multipart upload
    test_multipart_upload(
        &test_client.s3,
        &test_client.region,
        &credentials,
        &test_client.bucket_name,
        &multipart_filename,
    );

    // PUT an object via stream
    let streaming_filename = format!("streaming_test_file_{}", get_time().sec);
    test_put_object_stream_with_filename(
        &test_client.s3,
        &test_client.bucket_name,
        &streaming_filename,
        &"tests/sample-data/binary-file",
    );

    test_delete_object(
        &test_client.s3,
        &test_client.bucket_name,
        &multipart_filename,
    );

    test_delete_object(
        &test_client.s3,
        &test_client.bucket_name,
        &streaming_filename,
    )
}

fn test_multipart_upload(
    client: &S3Client,
    region: &Region,
    credentials: &AwsCredentials,
    bucket: &str,
    filename: &str,
) {
    let create_multipart_req = CreateMultipartUploadRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };

    // start the multipart upload and note the upload_id generated
    let response = client
        .create_multipart_upload(create_multipart_req)
        .sync()
        .expect("Couldn't create multipart upload");
    println!("{:#?}", response);
    let upload_id = response.upload_id.unwrap();

    // create 2 upload parts
    let create_upload_part = |body: Vec<u8>, part_number: i64| -> UploadPartRequest {
        UploadPartRequest {
            body: Some(body.into()),
            bucket: bucket.to_owned(),
            key: filename.to_owned(),
            upload_id: upload_id.to_owned(),
            part_number: part_number,
            ..Default::default()
        }
    };

    // minimum size for a non-final multipart upload part is 5MB
    let part_req1 = create_upload_part(vec!['a' as u8; 1024 * 1024 * 5], 1);
    let part_req2 = create_upload_part("foo".as_bytes().to_vec(), 2);

    // upload the parts and note the etags generated for them
    let mut completed_parts = Vec::new();
    {
        let part_number = part_req1.part_number;
        let response = client
            .upload_part(part_req1)
            .sync()
            .expect("Couldn't upload a file part");
        println!("{:#?}", response);
        completed_parts.push(CompletedPart {
            e_tag: response.e_tag.clone(),
            part_number: Some(part_number),
        });
    }
    // upload the second part via a pre-signed url
    {
        let options = PreSignedRequestOption {
            expires_in: Duration::from_secs(60 * 30),
        };
        let presigned_multipart_put = part_req2.get_presigned_url(region, credentials, &options);
        println!("presigned multipart put: {:#?}", presigned_multipart_put);
        let client = reqwest::Client::new();
        let res = client
            .put(&presigned_multipart_put)
            .body(String::from("foo"))
            .send()
            .expect("Multipart put with presigned url failed");
        assert_eq!(res.status(), http::StatusCode::OK);
        let e_tag = res.headers().get("ETAG").unwrap().to_str().unwrap();
        completed_parts.push(CompletedPart {
            e_tag: Some(e_tag.to_string()),
            part_number: Some(part_req2.part_number),
        });
    }

    // complete the multipart upload with the etags of the parts
    let completed_upload = CompletedMultipartUpload {
        parts: Some(completed_parts),
    };

    let complete_req = CompleteMultipartUploadRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        upload_id: upload_id.to_owned(),
        multipart_upload: Some(completed_upload),
        ..Default::default()
    };

    let response = client
        .complete_multipart_upload(complete_req)
        .sync()
        .expect("Couldn't complete multipart upload");
    println!("{:#?}", response);

    // Add copy upload part to this test
    // https://docs.aws.amazon.com/AmazonS3/latest/API/mpUploadUploadPartCopy.html
    let create_multipart_req2 = CreateMultipartUploadRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };
    let upload_multi_response = client
        .create_multipart_upload(create_multipart_req2)
        .sync()
        .expect("Couldn't create multipart upload2");
    println!("{:#?}", upload_multi_response);
    let upload_id2 = upload_multi_response.upload_id.unwrap();
    let upload_part_copy_req = UploadPartCopyRequest {
        key: filename.to_owned(),
        bucket: bucket.to_owned(),
        part_number: 1,
        upload_id: upload_id2.clone(),
        copy_source: format!("{}/{}", bucket, filename).to_owned(),
        ..Default::default()
    };
    let copy_response = client
        .upload_part_copy(upload_part_copy_req)
        .sync()
        .expect("Should have had copy part work");
    println!("copy response: {:#?}", copy_response);

    let upload_part_copy_req2 = UploadPartCopyRequest {
        key: filename.to_owned(),
        bucket: bucket.to_owned(),
        part_number: 2,
        upload_id: upload_id2.clone(),
        copy_source: format!("{}/{}", bucket, filename).to_owned(),
        ..Default::default()
    };
    let copy_response2 = client
        .upload_part_copy(upload_part_copy_req2)
        .sync()
        .expect("Should have had copy part work");
    println!("copy response2: {:#?}", copy_response2);

    // complete the upload_part_copy upload:
    let completed_parts_2 = vec![
        CompletedPart {
            e_tag: copy_response.copy_part_result.unwrap().e_tag.clone(),
            part_number: Some(1),
        },
        CompletedPart {
            e_tag: copy_response2.copy_part_result.unwrap().e_tag.clone(),
            part_number: Some(2),
        },
    ];

    let completed_upload2 = CompletedMultipartUpload {
        parts: Some(completed_parts_2),
    };

    let complete_req2 = CompleteMultipartUploadRequest {
        bucket: bucket.to_owned(),

        key: filename.to_owned(),
        upload_id: upload_id2.to_owned(),
        multipart_upload: Some(completed_upload2),
        ..Default::default()
    };

    let response2 = client
        .complete_multipart_upload(complete_req2)
        .sync()
        .expect("Couldn't complete multipart upload2");
    println!("{:#?}", response2);
}

fn test_delete_bucket(client: &S3Client, bucket: &str) {
    let delete_bucket_req = DeleteBucketRequest {
        bucket: bucket.to_owned(),
        ..Default::default()
    };

    let result = client.delete_bucket(delete_bucket_req).sync();
    println!("{:#?}", result);
    match result {
        Err(e) => match e {
            RusotoError::Unknown(ref e) => panic!(
                "Couldn't delete bucket because: {}",
                str::from_utf8(&e.body).unwrap()
            ),
            _ => panic!("Error from S3 different than expected"),
        },
        Ok(_) => (),
    }
}

fn test_put_object_with_filename(
    client: &S3Client,
    bucket: &str,
    dest_filename: &str,
    local_filename: &str,
) {
    let mut f = File::open(local_filename).unwrap();
    let mut contents: Vec<u8> = Vec::new();
    match f.read_to_end(&mut contents) {
        Err(why) => panic!("Error opening file to send to S3: {}", why),
        Ok(_) => {
            let req = PutObjectRequest {
                bucket: bucket.to_owned(),
                key: dest_filename.to_owned(),
                body: Some(contents.into()),
                ..Default::default()
            };
            let result = client.put_object(req).sync().expect("Couldn't PUT object");
            println!("{:#?}", result);
        }
    }
}

fn test_put_object_stream_with_filename(
    client: &S3Client,
    bucket: &str,
    dest_filename: &str,
    local_filename: &str,
) {
    let meta = ::std::fs::metadata(local_filename).unwrap();
    let fs = FsPool::default();
    let read_stream = fs.read(local_filename.to_owned());
    let req = PutObjectRequest {
        bucket: bucket.to_owned(),
        key: dest_filename.to_owned(),
        content_length: Some(meta.len() as i64),
        body: Some(StreamingBody::new(read_stream)),
        ..Default::default()
    };
    let result = client.put_object(req).sync().expect("Couldn't PUT object");
    println!("{:#?}", result);
}

fn test_head_object(client: &S3Client, bucket: &str, filename: &str) {
    let head_req = HeadObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };

    let result = client
        .head_object(head_req)
        .sync()
        .expect("Couldn't HEAD object");
    println!("{:#?}", result);
}

fn test_get_object(client: &S3Client, bucket: &str, filename: &str) {
    let get_req = GetObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };

    let result = client
        .get_object(get_req)
        .sync()
        .expect("Couldn't GET object");
    println!("get object result: {:#?}", result);

    let stream = result.body.unwrap();
    let body = stream.concat2().wait().unwrap();

    assert!(body.len() > 0);
}

fn test_get_object_blocking_read(client: &S3Client, bucket: &str, filename: &str) {
    let get_req = GetObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };

    let result = client
        .get_object(get_req)
        .sync()
        .expect("Couldn't GET object");
    println!("get object result: {:#?}", result);

    let mut stream = result.body.unwrap().into_blocking_read();
    let mut body = Vec::new();
    stream.read_to_end(&mut body).unwrap();

    assert!(body.len() > 0);
}

fn test_get_object_no_such_object(client: &S3Client, bucket: &str, filename: &str) {
    let get_req = GetObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };

    match client.get_object(get_req).sync() {
        Err(RusotoError::Service(GetObjectError::NoSuchKey(_))) => (),
        r => panic!("unexpected response {:?}", r),
    };
}

fn test_get_object_range(client: &S3Client, bucket: &str, filename: &str) {
    let get_req = GetObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        range: Some("bytes=0-1".to_owned()),
        ..Default::default()
    };

    let result = client
        .get_object(get_req)
        .sync()
        .expect("Couldn't GET object (range)");
    println!("\nget object range result: {:#?}", result);
    assert_eq!(result.content_length.unwrap(), 2);
}

fn test_copy_object(client: &S3Client, bucket: &str, filename: &str) {
    let req = CopyObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        copy_source: format!("{}/{}", bucket, filename),
        cache_control: Some("max-age=123".to_owned()),
        content_type: Some("application/json".to_owned()),
        metadata_directive: Some("REPLACE".to_owned()),
        ..Default::default()
    };

    let result = client
        .copy_object(req)
        .sync()
        .expect("Couldn't copy object");
    println!("{:#?}", result);
}

fn test_copy_object_utf8(client: &S3Client, bucket: &str, filename: &str) {
    let req = CopyObjectRequest {
        bucket: bucket.to_owned(),
        key: format!("{}", filename.to_owned()),
        copy_source: rusoto_s3::util::encode_key(format!("{}/{}", bucket, filename)),
        cache_control: Some("max-age=123".to_owned()),
        content_type: Some("application/json".to_owned()),
        metadata_directive: Some("REPLACE".to_owned()),
        ..Default::default()
    };

    let result = client
        .copy_object(req)
        .sync()
        .expect("Couldn't copy object (utf8)");
    println!("{:#?}", result);
}

fn test_delete_object(client: &S3Client, bucket: &str, filename: &str) {
    let del_req = DeleteObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };

    let result = client
        .delete_object(del_req)
        .sync()
        .expect("Couldn't delete object");
    println!("{:#?}", result);
}

fn list_items_in_bucket_paged_v1(client: &S3Client, bucket: &str) {
    let mut list_request = ListObjectsRequest {
        delimiter: Some("/".to_owned()),
        bucket: bucket.to_owned(),
        max_keys: Some(2),
        ..Default::default()
    };

    let response1 = client
        .list_objects(list_request.clone())
        .sync()
        .expect("list objects failed");
    println!("Items in bucket, page 1: {:#?}", response1);
    let contents1 = response1.contents.unwrap();
    assert!(response1.is_truncated.unwrap());
    assert_eq!(contents1.len(), 2);

    list_request.marker = Some(response1.next_marker.unwrap());
    list_request.max_keys = Some(1000);
    let response2 = client
        .list_objects(list_request)
        .sync()
        .expect("list objects failed");
    println!("Items in buckut, page 2: {:#?}", response2);
    let contents2 = response2.contents.unwrap();
    assert!(!response2.is_truncated.unwrap());
    assert!(contents1[1].key.as_ref().unwrap() < contents2[0].key.as_ref().unwrap());
}

// Assuming there's already more than three item in our test bucket:
fn list_items_in_bucket_paged_v2(client: &S3Client, bucket: &str) {
    let mut list_obj_req = ListObjectsV2Request {
        bucket: bucket.to_owned(),
        max_keys: Some(1),
        ..Default::default()
    };
    let result1 = client
        .list_objects_v2(list_obj_req.clone())
        .sync()
        .expect("list objects v2 failed");
    println!("Items in bucket, page 1: {:#?}", result1);
    assert!(result1.next_continuation_token.is_some());

    list_obj_req.continuation_token = result1.next_continuation_token;
    let result2 = client
        .list_objects_v2(list_obj_req)
        .sync()
        .expect("list objects v2 paging failed");
    println!("Items in bucket, page 2: {:#?}", result2);
    // For the second call it the token is in `continuation_token` not `next_continuation_token`
    assert!(result2.continuation_token.is_some());
    assert!(
        result1.contents.unwrap()[0].key.as_ref().unwrap()
            < result2.contents.unwrap()[0].key.as_ref().unwrap()
    );
}

fn test_put_bucket_cors(client: &S3Client, bucket: &str) {
    let cors_rules = vec![CORSRule {
        allowed_methods: vec!["PUT".to_owned(), "POST".to_owned(), "DELETE".to_owned()],
        allowed_origins: vec!["http://www.example.com".to_owned()],
        allowed_headers: Some(vec!["*".to_owned()]),
        max_age_seconds: Some(3000),
        expose_headers: Some(vec!["x-amz-server-side-encryption".to_owned()]),
        ..Default::default()
    }];

    let cors_configuration = CORSConfiguration {
        cors_rules: cors_rules,
    };

    let req = PutBucketCorsRequest {
        bucket: bucket.to_owned(),
        cors_configuration: cors_configuration,
        ..Default::default()
    };

    let result = client
        .put_bucket_cors(req)
        .sync()
        .expect("Couldn't apply bucket CORS");
    println!("{:#?}", result);
}

fn test_put_object_with_metadata(
    client: &S3Client,
    bucket: &str,
    dest_filename: &str,
    local_filename: &str,
    metadata: &HashMap<String, String>,
) {
    let mut f = File::open(local_filename).unwrap();
    let mut contents: Vec<u8> = Vec::new();
    match f.read_to_end(&mut contents) {
        Err(why) => panic!("Error opening file to send to S3: {}", why),
        Ok(_) => {
            let req = PutObjectRequest {
                bucket: bucket.to_owned(),
                key: dest_filename.to_owned(),
                body: Some(contents.into()),
                metadata: Some(metadata.clone()),
                ..Default::default()
            };
            let result = client.put_object(req).sync().expect("Couldn't PUT object");
            println!("{:#?}", result);
        }
    }
}

fn test_head_object_with_metadata(
    client: &S3Client,
    bucket: &str,
    filename: &str,
    metadata: &HashMap<String, String>,
) {
    let head_req = HeadObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };

    let result = client
        .head_object(head_req)
        .sync()
        .expect("Couldn't HEAD object");
    println!("{:#?}", result);

    let head_metadata = result.metadata.as_ref().expect("No metadata available");
    assert_eq!(metadata, head_metadata);
}

fn test_get_object_with_metadata(
    client: &S3Client,
    bucket: &str,
    filename: &str,
    metadata: &HashMap<String, String>,
) {
    let get_req = GetObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };

    let result = client
        .get_object(get_req)
        .sync()
        .expect("Couldn't GET object");
    println!("get object result: {:#?}", result);

    let head_metadata = result.metadata.as_ref().expect("No metadata available");
    assert_eq!(metadata, head_metadata);
}

fn test_get_object_with_presigned_url(
    region: &Region,
    credentials: &AwsCredentials,
    bucket: &str,
    filename: &str,
) {
    let req = GetObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };
    let presigned_url = req.get_presigned_url(region, credentials, &Default::default());
    println!("get object presigned url: {:#?}", presigned_url);
    let mut res = reqwest::get(&presigned_url).expect("Couldn't get object via presigned url");
    assert_eq!(res.status(), http::StatusCode::OK);
    let size = res.content_length().unwrap_or(0);
    assert!(size > 0);
    let mut buf: Vec<u8> = vec![];
    res.copy_to(&mut buf).expect("Copying failed");
    assert!(buf.len() > 0);
}

fn test_get_object_with_expired_presigned_url(
    region: &Region,
    credentials: &AwsCredentials,
    bucket: &str,
    filename: &str,
) {
    let req = GetObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };
    let opt = PreSignedRequestOption {
        expires_in: ::std::time::Duration::from_secs(1),
    };
    let presigned_url = req.get_presigned_url(region, credentials, &opt);
    ::std::thread::sleep(::std::time::Duration::from_secs(2));
    println!("get object presigned url: {:#?}", presigned_url);
    let res = reqwest::get(&presigned_url).expect("Presigned url failure");
    assert_eq!(res.status(), http::StatusCode::FORBIDDEN);
}

fn test_put_object_with_presigned_url(
    region: &Region,
    credentials: &AwsCredentials,
    bucket: &str,
    filename: &str,
) {
    let req = PutObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };
    let presigned_url = req.get_presigned_url(region, credentials, &Default::default());
    println!("put object presigned url: {:#?}", presigned_url);
    let mut map = HashMap::new();
    map.insert("test", "data");
    let client = reqwest::Client::new();
    let res = client
        .put(&presigned_url)
        .json(&map)
        .send()
        .expect("Put obj with presigned url failed");
    assert_eq!(res.status(), http::StatusCode::OK);
}

fn test_delete_object_with_presigned_url(
    region: &Region,
    credentials: &AwsCredentials,
    bucket: &str,
    filename: &str,
) {
    let req = DeleteObjectRequest {
        bucket: bucket.to_owned(),
        key: filename.to_owned(),
        ..Default::default()
    };
    let presigned_url = req.get_presigned_url(region, credentials, &Default::default());
    println!("delete object presigned url: {:#?}", presigned_url);
    let client = reqwest::Client::new();
    let res = client
        .delete(&presigned_url)
        .send()
        .expect("Delete of presigned url obj failed");
    assert_eq!(res.status(), http::StatusCode::NO_CONTENT);
}
