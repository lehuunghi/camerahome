# CameraHome — bộ ghi độc lập

Bản mới dùng **go2rtc + FFmpeg + phát hiện chuyển động/tiếng khóc riêng**, không cần chạy Frigate. Ghi nguyên luồng HD, chia đoạn tối đa khoảng 3 phút, hàng đợi SQLite cho R2 và Google Drive. Xem [hướng dẫn bộ ghi độc lập](engine/README.md) và [Docker Compose mới](deploy/native.compose.yaml).

Các mục bên dưới mô tả cách cài Frigate cũ và được giữ để tham khảo/khôi phục.

# CameraHome

Giao diện quản lý camera Tapo bằng Rust/Axum, dùng Frigate để nhận RTSP, phát hiện sự kiện và ghi hình. Kho chứa mã ứng dụng, script xử lý video và các mẫu dịch vụ đang dùng trên máy chủ. Video, thông tin đăng nhập và trạng thái vận hành được lưu ngoài Git.

## Chức năng hiện tại

- Xem trực tiếp có tiếng, tự kết nối lại khi mất luồng hoặc quay lại tab.
- Lọc ồn tự bật khi mở tiếng live; không hiển thị nút bật/tắt riêng.
- Zoom, kéo khung hình, toàn màn hình; PTZ trái/phải/lên/xuống nếu camera hỗ trợ.
- Chất lượng live tiết kiệm hoặc HD 1080p. Luồng HD có thể cần chuyển mã bằng CPU.
- Thêm, sửa, gỡ camera, sửa kết nối; lưu MAC và tự dò IP mới trong cùng LAN.
- Chọn ghi chuyển động, tiếng khóc hoặc kết hợp; chọn luồng chính HD/độ nét gốc hoặc luồng phụ.
- Xuất đoạn tối đa khoảng 180 giây; sự kiện ngắn có thể tạo đoạn ngắn hơn.
- Nén AV1/AAC giữ độ phân giải; có cơ chế sao chép luồng dự phòng khi không xuất AV1 phù hợp.
- Xem lại theo ngày Việt Nam, timeline, tự chuyển đoạn; tua nhanh/chậm và tải clip.
- Chọn nguồn xem lại trong Quản trị: máy chủ, Google Drive hoặc Cloudflare R2. Lựa chọn được ghi nhớ theo trình duyệt.
- Danh sách bản ghi đám mây đã xác nhận được lưu riêng; tận dụng file còn trên máy chủ làm bộ đệm sau khi xác nhận bản sao đám mây.
- Tài khoản, quyền theo camera và thao tác; đăng nhập/đăng xuất, phiên lưu bền, mật khẩu mới tối thiểu 6 ký tự.
- Đổi tên website, tiêu đề tab và trang đăng nhập từ Quản trị.
- Đặt ngày lưu riêng cho Drive/R2 và bản video đã sao lưu còn trên máy chủ.
- Xóa bản ghi theo phạm vi; xóa toàn bộ video máy chủ qua tác vụ có xác nhận.

## Thành phần

| Đường dẫn | Vai trò |
| --- | --- |
| `src/main.rs` | Khởi tạo ứng dụng, API camera và bản ghi cục bộ |
| `src/accounts.rs` | Đăng nhập, phiên bền, phân quyền, đổi tên website |
| `src/manage.rs` | Camera, live/PTZ, OAuth và nhập mã Drive, yêu cầu dọn video |
| `src/mac.rs` | Nhận diện MAC và cập nhật kết nối khi IP thay đổi |
| `src/cloud.rs` | Drive/R2, sao lưu, hạn lưu, danh sách và phát video đám mây |
| `web/` | Giao diện chính và trang đăng nhập |
| `scripts/group-recordings.py` | Chọn bản ghi có sự kiện, ghép/nén clip |
| `scripts/discover-lan.py` | Dò thiết bị RTSP và bảng MAC trên LAN của host |
| `scripts/purge-recordings.py` | Dừng ghi, dọn video/chỉ mục/bộ đệm rồi bật ghi lại |
| `deploy/` | Các file systemd service/timer/path |
| `compose.yaml`, `Dockerfile` | Container CameraHome |

`backup.sh` là cơ chế sao lưu cũ. Bản hiện tại dùng worker trong Rust; không bật đồng thời Cron cũ và worker mới.

## Điều kiện triển khai

- Linux, Docker Compose, Python 3 và iproute2 trên host.
- Frigate đã chạy, tên container mặc định `frigate`, có go2rtc và API nội bộ ở cổng 5000.
- Cấu hình Frigate tương thích với các mục `record.continuous`, `record.motion`, audio/review và API lưu cấu hình của ứng dụng.
- Script nén hiện dùng FFmpeg trong container Frigate tại `/usr/lib/ffmpeg/7.0/bin/ffmpeg`, cần codec `libsvtav1`. Nếu image Frigate khác, kiểm tra và sửa đường dẫn trước.
- Compose của Frigate được giữ ở `/opt/frigate/compose.yaml`. Tác vụ xóa toàn bộ dùng file này để tạo lại container và loại bỏ bộ đệm video cũ.
- Camera Tapo có Camera Account/RTSP; nhận diện tiếng khóc cần luồng âm thanh. Kết quả phát hiện phụ thuộc cấu hình Frigate và môi trường.
- Dò MAC cần host cùng mạng LAN trực tiếp với camera. Mặc định mạng `192.168.88.0/24`.

AV1 giúp giảm dung lượng nhưng dùng nhiều CPU, đặc biệt với 1080p/2K và VPS không có GPU mã hóa. Tác vụ nén chạy trong container Frigate nên CPU của nó được tính vào container này.

## Thư mục vận hành ngoài Git

| Host | Container CameraHome | Nội dung |
| --- | --- | --- |
| `/opt/frigate/config` | `/frigate-config` (chỉ đọc) | Cấu hình Frigate |
| `/opt/frigate/storage/camerahome-clips` | `/recordings` | Video đã xuất/nén |
| `/opt/frigate/backup-status` | `/backup-status` (chỉ đọc) | Trạng thái cơ chế cũ |
| `/opt/camerahome-data` | `/app-data` | Phiên, tên website, cấu hình kho, token, chỉ mục và trạng thái |
| `/opt/camerahome-auth` | `/nginx-auth` | Hash tài khoản, phân quyền và bí mật xác thực proxy |

Thư mục dữ liệu và xác thực cần được bảo vệ bằng quyền truy cập thích hợp, thường 0700; file bí mật 0600. Sao lưu riêng các thư mục này nếu cần khôi phục máy chủ. Không đưa token Drive, khóa R2, mật khẩu/hash tài khoản, cấu hình RTSP thật hoặc video lên kho công khai.

## Cài ứng dụng trên aaPanel

Đây là hướng dẫn triển khai mã nguồn với Frigate đã có sẵn, không phải bản sao toàn bộ VPS.

1. Đặt checkout tại `/opt/camerahome`; tạo các thư mục dữ liệu theo bảng trên.
2. Sửa `compose.yaml` cho đúng `PUBLIC_ORIGIN`, `CAMERA_SUBNET` và đường dẫn host.
3. Tạo mạng Docker `camera_backend` nếu chưa có; khai báo mạng này trong cả Compose Frigate và CameraHome. Dịch vụ Frigate cần có alias `frigate`. Giữ cổng 5000 của Frigate ở mạng nội bộ.
4. Chuẩn bị xác thực trước khi mở website:
   - `/opt/camerahome-auth/proxy-token`: chuỗi ngẫu nhiên bí mật, ít nhất 32 ký tự.
   - `/opt/camerahome-auth/users`: có dòng `admin:<hash SHA-512 crypt>`; tạo hash bằng `openssl passwd -6`.
   - Không ghi đè các file xác thực đã có khi cập nhật.
5. Build và chạy:

   ```sh
   cd /opt/camerahome
   docker compose up -d --build
   ```

6. Trong aaPanel, bật HTTPS và reverse proxy tới `http://127.0.0.1:8180`. Cho phép WebSocket và thời gian chờ đủ dài.
7. Nginx phải ghi đè các header sau cho mọi route được proxy, kể cả live và OAuth:
   - `X-CameraHome-Proxy`: giá trị bí mật trùng file proxy-token.
   - `X-CameraHome-User`: chuỗi rỗng; không tin header do trình duyệt gửi.
   - `X-CameraHome-Client`: địa chỉ client cho giới hạn lần đăng nhập.
8. Tắt access log riêng cho `/drive/callback` để không lưu mã OAuth. Không công bố cổng 8180 ra Internet.
9. Đăng nhập bằng biểu mẫu CameraHome tại `/login`. Bản hiện tại xác thực tài khoản và phiên trong ứng dụng, không cần HTTP Basic Auth của Nginx.

Cấu hình Nginx thực tế chứa bí mật proxy và chứng chỉ được giữ trên máy chủ; kho chỉ mô tả yêu cầu tích hợp.

## Cài các dịch vụ nền

Kiểm tra đường dẫn và mạng trong mẫu trước khi cài. Các mẫu dùng checkout ở `/opt/camerahome`.

```sh
sudo install -m 644 deploy/camerahome-*.service /etc/systemd/system/
sudo install -m 644 deploy/camerahome-*.timer /etc/systemd/system/
sudo install -m 644 deploy/camerahome-*.path /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now camerahome-clips.timer camerahome-lan.timer camerahome-purge.path
```

- `camerahome-clips.timer`: chạy lại sau khi lượt xuất clip trước kết thúc, khoảng 15 giây.
- `camerahome-lan.timer`: dò LAN khoảng mỗi phút. Sửa `CAMERA_SUBNET` trong service nếu đổi mạng.
- `camerahome-purge.path`: theo dõi yêu cầu xóa do admin gửi từ website. Không chạy script purge thủ công để thay bước xác nhận.

## Google Drive và Cloudflare R2

Sao lưu tự chạy trong ứng dụng khoảng mỗi 5 phút. File cần hoàn tất ít nhất 60 giây; ứng dụng chọn file trước khi gọi rclone. Lượt dài hoặc bị giới hạn API có thể kết thúc muộn hơn. Có giới hạn tốc độ gọi Drive, thử lại và trạng thái tiến trình/lỗi trong Quản trị.

### Drive

Quản trị chỉ hỗ trợ kết nối bằng OAuth riêng. Không cần cài rclone trên máy người dùng.

- OAuth riêng: bật Drive API, tạo Web application client và thêm `PUBLIC_ORIGIN/drive/callback` làm redirect URI, rồi kết nối trên website.
- Bản sao nằm dưới `<thư mục đã chọn>/recordings/YYYY-MM-DD/HH/<camera>/MM.SS.mp4`.

### R2

Tạo bucket và khóa có quyền đọc/ghi/xóa trong bucket, rồi nhập Account ID, bucket, prefix, Access Key ID và Secret Access Key trong Quản trị. Ứng dụng dùng endpoint R2 S3 tiêu chuẩn. Không cần bucket công khai.

### Xem lại đám mây

Chọn nguồn trong Quản trị; chọn ngày/camera và xem tại mục Xem lại. Thời gian trên giao diện là giờ Việt Nam, đường dẫn file dùng UTC.

Danh sách đám mây đã xác nhận được lưu ở app-data. Nếu bản nén tương ứng còn trên máy chủ và dung lượng khớp bản sao đã xác nhận, ứng dụng dùng nó làm bộ đệm phát. Khi không có bộ đệm, ứng dụng đọc từ kho qua rclone. Phát theo byte range hỗ trợ tua và tải xuống; quyền theo camera vẫn được kiểm tra. Khi Drive giới hạn API, danh sách có thể cập nhật chậm và giao diện giữ kết quả đã tải được.

## Hạn lưu và xóa

- Drive: 0 ngày = không tự xóa. R2 có thời gian lưu từ 1 đến 3650 ngày khi sao lưu được bật.
- Bản video đã sao lưu trên máy chủ: 0 ngày = không tự xóa. Chỉ dọn file quá hạn khi các kho đã kết nối và đang bật đều xác nhận có bản sao đủ dung lượng.
- Bật cả Drive và R2 thì cần đủ hai bản sao trước khi tự dọn bản trên máy chủ.
- Hạn lưu tính theo thời điểm ghi trong tên file. Chỉ dọn phạm vi bản ghi của ứng dụng.
- Video gốc của Frigate có chính sách giữ riêng theo từng camera; không đồng nhất với hạn lưu video đã nén.
- Nút xóa toàn bộ yêu cầu nhập `XOA TAT CA`, tạm dừng ghi, dọn video gốc/nén/xuất và chỉ mục, tạo lại Frigate để bỏ bộ đệm, rồi bật ghi tiếp. Nó giữ bản sao Drive/R2; video mới có thể xuất hiện sau khi ghi được bật lại.

## Cập nhật và kiểm tra

```sh
git fetch origin main
git checkout --detach origin/main
docker compose up -d --build
```

Kiểm tra diff cục bộ trước khi checkout; không dùng lệnh reset để bỏ cấu hình riêng. Sau cập nhật, kiểm tra đăng nhập, live có tiếng, ghi sự kiện, xem/tua từng nguồn đã kết nối và trạng thái sao lưu. Các lần cập nhật chỉ tài liệu hoặc mẫu dịch vụ không cần rebuild ứng dụng nếu chưa thay mã.

```sh
cargo test --release
python3 -m py_compile scripts/*.py
systemctl status camerahome-clips.timer camerahome-lan.timer camerahome-purge.path
```

### Công tắc sao lưu

Trong Quản trị, mục **Bật/tắt sao lưu** có công tắc tổng và lựa chọn riêng cho máy chủ, Google Drive, Cloudflare R2. Kết nối kho và thời gian giữ video nằm riêng bên dưới. Cấu hình cũ mặc định giữ nguyên lưu máy chủ và trạng thái đám mây.

Tắt lưu máy chủ vẫn cho phép tạo file tạm để tải lên các kho đang bật. File mới ở chế độ này không xuất hiện trong nguồn xem lại Máy chủ và chỉ được dọn sau khi xác nhận đủ dung lượng trên mọi kho đang bật. Video cũ không bị xóa bởi thao tác tắt; vẫn theo hạn lưu đã đặt. Không có kho nào bật hoặc tắt công tắc tổng sẽ dừng tạo video nén mới; Frigate vẫn giữ bộ đệm gốc theo cấu hình camera. Công tắc không xóa dữ liệu và không chặn xem video đã lưu. Tiến trình tải lên đang chạy kiểm tra công tắc mỗi 2 giây; bộ xử lý video kiểm tra trước mỗi đoạn. Worker sao lưu kiểm tra mỗi 5 phút.

