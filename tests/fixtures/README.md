# Test data / 测试数据

`cpython-encrypted.zip.hex` encodes the pre-generated encrypted ZIP `DecryptionTests.data` from [CPython v3.10.0 test_zipfile.py](https://github.com/python/cpython/blob/v3.10.0/Lib/test/test_zipfile.py). The original authors and license are acknowledged in [PSF-LICENSE.txt](PSF-LICENSE.txt). No Python test code is executed or included in the application.

该文件以十六进制保存 CPython 的预生成加密 ZIP 样例。ArchiveFlow 测试只将其作为字节流拆分和合并，不解密、不解压，也不读取其内部文件目录。PSF 许可证仅适用于该第三方样例；本项目的发行许可证仍待发布者确定。

Tests decode the hex into a temporary, owned ZIP fixture and compare the original and merged byte hashes. The application never interprets archive semantics. Tests require no network access.
