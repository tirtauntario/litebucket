# Rails Active Storage end-to-end checks against storlite (run with
# `bin/rails runner`). Exits non-zero on the first failed check.
require "net/http"
require "securerandom"
require "digest"
require "aws-sdk-s3"

raise "refusing non-local endpoint" unless ENV.fetch("STORLITE_ENDPOINT").match?(%r{\Ahttps?://127\.0\.0\.1:})
CA = ENV["STORLITE_CA_BUNDLE"]
def http_start(uri, &blk) = Net::HTTP.start(uri.host, uri.port, use_ssl: uri.scheme == "https", ca_file: CA, &blk)

$passed = 0
def check(name)
  ok = yield
  puts "#{ok ? 'ok  ' : 'FAIL'} #{name}"
  exit 1 unless ok
  $passed += 1
rescue => e
  puts "FAIL #{name}: #{e.class}: #{e.message}"
  exit 1
end

puts "rails #{Rails.version}, activestorage #{ActiveStorage.version}, aws-sdk-s3 #{Aws::S3::GEM_VERSION}"
service = ActiveStorage::Blob.service
service.send(:client).client.create_bucket(bucket: ENV.fetch("STORLITE_RAILS_BUCKET"))

ActiveRecord::Schema.define { create_table(:documents, force: true) { |t| t.string :title } } rescue nil
class Document < ActiveRecord::Base
  has_one_attached :file
end

data = SecureRandom.random_bytes(150_000)
doc = Document.create!(title: "private")
doc.file.attach(io: StringIO.new(data), filename: "report é.pdf", content_type: "application/pdf")
blob = doc.file.blob

check("private attachment upload (checksum #{blob.checksum})") { service.exist?(blob.key) }
check("download") { doc.file.download.b == data.b }
check("range download") { service.download_chunk(blob.key, 100..199).b == data.b[100..199] }
check("streaming download") do
  out = +"".b
  service.download(blob.key) { |chunk| out << chunk }
  out == data.b
end

url = doc.file.url(disposition: :attachment)
check("private URL is presigned") { url.include?("X-Amz-Signature=") }
res = http_start(URI(url)) { |h| h.request(Net::HTTP::Get.new(URI(url))) }
check("presigned GET via Rails URL") { res.code == "200" && res.body.b == data.b }
check("content-disposition override") { res["content-disposition"].to_s.include?("attachment") }

# Direct upload: blob record first, then a signed PUT with Rails' headers.
direct = SecureRandom.random_bytes(80_000)
dblob = ActiveStorage::Blob.create_before_direct_upload!(
  filename: "direct.bin", byte_size: direct.bytesize,
  checksum: Digest::MD5.base64digest(direct), content_type: "application/octet-stream"
)
put_url = dblob.service_url_for_direct_upload(expires_in: 5.minutes)
headers = dblob.service_headers_for_direct_upload
uri = URI(put_url)
req = Net::HTTP::Put.new(uri)
headers.each { |k, v| req[k] = v }
req.body = direct
put_res = http_start(uri) { |h| h.request(req) }
check("direct upload PUT (#{headers.keys.join(', ')})") { put_res.code == "200" }
check("direct-uploaded blob readable") { dblob.download.b == direct.b }

# A direct upload whose bytes do not match the signed Content-MD5 is rejected.
bad = ActiveStorage::Blob.create_before_direct_upload!(
  filename: "bad.bin", byte_size: 4, checksum: Digest::MD5.base64digest("good"), content_type: "text/plain"
)
breq = Net::HTTP::Put.new(URI(bad.service_url_for_direct_upload(expires_in: 5.minutes)))
bad.service_headers_for_direct_upload.each { |k, v| breq[k] = v }
breq.body = "evil"
bres = http_start(uri) { |h| h.request(breq) }
check("tampered direct upload rejected (#{bres.code})") { bres.code.start_with?("4") && !service.exist?(bad.key) }

# Prefix deletion (used for variants).
%w[a b c].each { |n| service.upload("variants/#{blob.key}/#{n}", StringIO.new(n)) }
service.upload("variants-other/keep", StringIO.new("keep"))
service.delete_prefixed("variants/#{blob.key}/")
check("delete_prefixed") do
  %w[a b c].none? { |n| service.exist?("variants/#{blob.key}/#{n}") } && service.exist?("variants-other/keep")
end

doc.file.purge
check("purge removes the object") { !service.exist?(blob.key) }
puts "rails: #{$passed} passed"
