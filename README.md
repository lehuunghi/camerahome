# CameraHome

Bảng quản lý nội bộ bằng Rust cho Frigate. CameraHome hiển thị ảnh mới nhất và trạng thái nhận hình, phát lại đoạn MP4 đã lưu, và báo kết quả lượt sao lưu Google Drive gần nhất. Frigate xử lý RTSP và ghi hình. CameraHome hiện đọc dữ liệu. Thêm camera, chỉnh vùng chuyển động và các thay đổi đầu ghi vẫn thực hiện trong Frigate. Giao diện hiện ảnh cập nhật định kỳ, không phát trực tiếp liên tục.

## Cài trên Mini PC có aaPanel

1. Cài và cấu hình Frigate trước. Với mỗi camera Tapo, tạo Camera Account trong app Tapo, thử RTSP trên LAN, rồi thêm camera trong Frigate. Cấu hình luồng có vai trò record và detect. Với phiên bản Frigate có chính sách continuous/motion, đặt toàn cục như sau và bỏ thiết lập ghi liên tục riêng của từng camera.

   ```yaml
   record:
     enabled: true
     continuous:
       days: 0
     motion:
       days: 3
   ```

2. Chạy các lệnh sau với tài khoản có quyền Docker. Nếu Frigate dùng tên container khác, sửa tên trong lệnh và URL của Compose. Mạng phải được khai báo trong Compose của Frigate để kết nối còn tồn tại khi tạo lại container. Cổng 5000 của Frigate chỉ dùng nội bộ mạng Docker.

   ```sh
   docker network create camera_backend
   docker network connect --alias frigate camera_backend frigate
   docker compose up -d --build
   curl -f http://127.0.0.1:8180/api/cameras
   ```

3. Với cấu hình Frigate quản lý qua Compose, thêm mạng camera_backend vào danh sách mạng của dịch vụ frigate, giữ các mạng đang dùng và khai báo camera_backend là external: true ở mục networks cấp cao nhất. Nếu thư mục Frigate ngoài host khác mẫu, sửa đường dẫn trong compose.yaml và backup.sh.
4. Trong aaPanel, tạo website nội bộ, bật HTTPS và bảo vệ toàn bộ website bằng xác thực Nginx trước khi tạo reverse proxy đến http://127.0.0.1:8180. Kiểm tra truy cập chưa đăng nhập vào /, /api/cameras và /media/ đều bị chặn. CameraHome dựa vào lớp xác thực này. Trường hợp truy cập từ xa, ưu tiên VPN vào LAN.
5. Cài rclone trên host. Dùng rclone config để tạo kết nối Google Drive tên gdrive bằng cùng tài khoản Linux chạy Cron. Kiểm tra bằng rclone lsd gdrive:. Trong aaPanel Cron tạo Shell Script chạy mỗi 5 phút và gọi đường dẫn tuyệt đối tới backup.sh. Tác vụ chỉ sao chép các đoạn Frigate đã lưu, giữ bản đã tải trên Drive khi Frigate dọn cục bộ. Trạng thái là kết quả của cả lượt chạy, không phải xác nhận riêng từng clip.
6. Thử từng camera bằng cách đi qua vùng quan sát. Xác nhận Frigate tạo đoạn MP4, CameraHome phát lại được đoạn đó, và Drive nhận file sau lượt Cron. Ngày lọc trên web dùng UTC theo cấu trúc thư mục của Frigate.

Không đưa cấu hình RTSP thật, token rclone, dữ liệu video hoặc mật khẩu lên GitHub.
