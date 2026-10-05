# SDK-02 (Ruby): default-checksum, metadata, listing, presign, and forced
# multipart flows against a local litebucket. Run via scripts/interop.sh.
require 'minitest/autorun'
require 'aws-sdk-s3'
require 'securerandom'
require 'digest'
require 'net/http'

ENDPOINT = ENV.fetch('LITEBUCKET_ENDPOINT')
raise "refusing non-local endpoint #{ENDPOINT}" unless ENDPOINT.match?(%r{\Ahttps?://127\.0\.0\.1:})
CA = ENV['LITEBUCKET_CA_BUNDLE']

class RubySdkTest < Minitest::Test
  def self.client
    @client ||= Aws::S3::Client.new(
      endpoint: ENDPOINT, region: ENV.fetch('LITEBUCKET_REGION'), force_path_style: true,
      credentials: Aws::Credentials.new(ENV.fetch('LITEBUCKET_KEY_ID'), ENV.fetch('LITEBUCKET_SECRET')),
      **(CA ? { ssl_ca_bundle: CA } : {})
    )
  end

  def s3 = self.class.client

  def bucket
    @bucket ||= "ruby-#{SecureRandom.hex(4)}".tap { |b| s3.create_bucket(bucket: b) }
  end

  def test_versions_recorded
    puts "aws-sdk-s3 #{Aws::S3::GEM_VERSION}, aws-sdk-core #{Aws::CORE_GEM_VERSION}, ruby #{RUBY_VERSION}"
    puts "request_checksum_calculation=#{s3.config.request_checksum_calculation} response_checksum_validation=#{s3.config.response_checksum_validation}"
  end

  def test_object_round_trip_with_default_checksums
    body = SecureRandom.random_bytes(200_000)
    r = s3.put_object(bucket: bucket, key: 'dir/a.bin', body: body, content_type: 'application/x-test',
                      metadata: { 'owner' => 'ruby', 'note' => 'é ok' }, cache_control: 'no-cache')
    assert_equal %("#{Digest::MD5.hexdigest(body)}"), r.etag
    g = s3.get_object(bucket: bucket, key: 'dir/a.bin', checksum_mode: 'ENABLED')
    assert_equal body, g.body.read.b
    assert_equal 'application/x-test', g.content_type
    assert_equal 'ruby', g.metadata['owner']
    assert_equal 'no-cache', g.cache_control
    h = s3.head_object(bucket: bucket, key: 'dir/a.bin', checksum_mode: 'ENABLED')
    assert_equal 200_000, h.content_length
    refute_nil(h.checksum_crc32 || h.checksum_crc64nvme || h.checksum_crc32c, 'a checksum is stored')
    s3.put_object(bucket: bucket, key: 'dir/a.bin', body: 'overwritten')
    assert_equal 'overwritten', s3.get_object(bucket: bucket, key: 'dir/a.bin').body.read
    assert_equal 'over', s3.get_object(bucket: bucket, key: 'dir/a.bin', range: 'bytes=0-3').body.read
  end

  def test_explicit_checksum_algorithms
    # CRC32C/CRC64NVME need the optional aws-crt gem in the Ruby SDK.
    %w[CRC32 SHA1 SHA256].each do |alg|
      s3.put_object(bucket: bucket, key: "sum-#{alg}", body: "data for #{alg}", checksum_algorithm: alg)
      g = s3.get_object(bucket: bucket, key: "sum-#{alg}", checksum_mode: 'ENABLED')
      assert_equal "data for #{alg}", g.body.read
    end
  end

  def test_listing_pagination_and_delimiters
    keys = (1..7).map { |i| "list/#{i}" } + ['list/sub/x', 'other']
    keys.each { |k| s3.put_object(bucket: bucket, key: k, body: k) }
    pages = s3.list_objects_v2(bucket: bucket, prefix: 'list/', max_keys: 2).each_page.to_a
    assert_operator pages.size, :>=, 4
    assert_equal keys.grep(/^list\//).sort, pages.flat_map { |p| p.contents.map(&:key) }.sort
    d = s3.list_objects_v2(bucket: bucket, prefix: 'list/', delimiter: '/')
    assert_equal ['list/sub/'], d.common_prefixes.map(&:prefix)
  end

  def test_copy_delete_and_batch_delete
    s3.put_object(bucket: bucket, key: 'src', body: 'copy me', metadata: { 'k' => 'v' })
    s3.copy_object(bucket: bucket, key: 'dst', copy_source: "#{bucket}/src")
    assert_equal 'copy me', s3.get_object(bucket: bucket, key: 'dst').body.read
    s3.delete_object(bucket: bucket, key: 'src')
    assert_raises(Aws::S3::Errors::NoSuchKey) { s3.get_object(bucket: bucket, key: 'src') }
    s3.delete_object(bucket: bucket, key: 'src') # idempotent
    s3.put_object(bucket: bucket, key: 'b1', body: '1')
    r = s3.delete_objects(bucket: bucket, delete: { objects: [{ key: 'dst' }, { key: 'b1' }, { key: 'nope' }] })
    assert_equal %w[b1 dst nope], r.deleted.map(&:key).sort
  end

  def test_conditional_put
    s3.put_object(bucket: bucket, key: 'cond', body: 'v1', if_none_match: '*')
    assert_raises(Aws::S3::Errors::PreconditionFailed) do
      s3.put_object(bucket: bucket, key: 'cond', body: 'v2', if_none_match: '*')
    end
  end

  def test_forced_multipart_upload_and_download
    path = File.join(ENV.fetch('INTEROP_WORK'), "ruby-big-#{SecureRandom.hex(4)}")
    File.binwrite(path, SecureRandom.random_bytes(17 * 1024 * 1024))
    obj = Aws::S3::Resource.new(client: s3).bucket(bucket).object('big.bin')
    assert obj.upload_file(path, multipart_threshold: 5 * 1024 * 1024)
    h = s3.head_object(bucket: bucket, key: 'big.bin')
    assert_match(/-\d+"\z/, h.etag, 'multipart ETag')
    out = "#{path}.down"
    obj.download_file(out, mode: 'auto', chunk_size: 5 * 1024 * 1024)
    assert_equal Digest::SHA256.file(path).hexdigest, Digest::SHA256.file(out).hexdigest
    # Low-level multipart with listing and abort.
    up = s3.create_multipart_upload(bucket: bucket, key: 'aborted')
    s3.upload_part(bucket: bucket, key: 'aborted', upload_id: up.upload_id, part_number: 1, body: 'x')
    assert_equal 1, s3.list_multipart_uploads(bucket: bucket).uploads.count { |u| u.key == 'aborted' }
    s3.abort_multipart_upload(bucket: bucket, key: 'aborted', upload_id: up.upload_id)
    assert_raises(Aws::S3::Errors::NoSuchUpload) { s3.list_parts(bucket: bucket, key: 'aborted', upload_id: up.upload_id) }
  end

  def test_presigned_urls
    signer = Aws::S3::Presigner.new(client: s3)
    put = URI(signer.presigned_url(:put_object, bucket: bucket, key: 'presigned.txt', expires_in: 300))
    http = ->(u, &blk) { Net::HTTP.start(u.host, u.port, use_ssl: u.scheme == 'https', ca_file: CA, &blk) }
    res = http.(put) { |h| h.request(Net::HTTP::Put.new(put).tap { |r| r.body = 'via presign' }) }
    assert_equal '200', res.code
    get = URI(signer.presigned_url(:get_object, bucket: bucket, key: 'presigned.txt', expires_in: 300))
    assert_equal 'via presign', http.(get) { |h| h.request(Net::HTTP::Get.new(get)).body }
  end

  def test_errors_map_to_sdk_exceptions
    assert_raises(Aws::S3::Errors::NoSuchBucket) { s3.list_objects_v2(bucket: 'does-not-exist-123') }
    assert_raises(Aws::S3::Errors::BucketAlreadyOwnedByYou) { s3.create_bucket(bucket: bucket) }
    assert_raises(Aws::S3::Errors::NotFound) { s3.head_object(bucket: bucket, key: 'missing') }
  end
end
