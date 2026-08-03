use buffa_build::{BytesRepr, Config, MapRepr, PointerRepr, ReflectMode, RepeatedRepr, StringRepr};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "proto";
    println!("cargo:rerun-if-changed=proto");

    // 统一正斜杠: Windows 宿主上 walkdir 产出反斜杠路径,剥掉 include 前缀后
    // 与 descriptor set 里的正斜杠文件名对不上,codegen 会报 FileNotFound。
    // 只在 Windows 宿主替换(build script 恒为宿主编译,cfg!(windows) 即宿主判定);
    // 其它宿主的路径本就是正斜杠,交叉编译到 Windows 目标也不需要。
    let files: Vec<String> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "proto"))
        .map(|e| {
            let p = e.path().to_string_lossy();
            if cfg!(windows) { p.replace('\\', "/") } else { p.into_owned() }
        })
        .collect();

    // buffa-build 的全部全局开关在此显式固定,包括与当前默认值一致的项:
    // 升级 buffa 时默认值变动不会悄悄改变生成物(0.9 已发生过一次这样的
    // 变更——单数 message 字段的默认表示从 Box 改为 Inline)。
    //
    // 有意不调用的 API(不存在可固定的"默认值",调用本身即引入行为):
    // - `out_dir` / `use_buf` / `descriptor_set`: 走默认 $OUT_DIR + protoc;
    // - `extern_path` / `override_feature_in` / `open_enums_in` /
    //   `unbox_oneof*` / `*_type_custom*` / `*_type_in` / `*_attribute` /
    //   `type_name_prefix`: 按路径的定制规则,本项目一条都不需要;
    // - `json/views/text/reflect_feature_name`: 仅在特性门控开启时有意义。
    Config::new()
        .files(&files)
        .includes(&[root])
        .include_file("_all.rs") // 单文件 include,省掉手写模块树
        // ---- 生成哪些代码面 ----
        .generate_views(true) // mark 阶段的主路径: 零拷贝 view 解码
        .lazy_views(false) // 实测无收益(热点是 oneof/map,强制走 eager),生成代码 +80%
        .generate_json(false) // 只用 binary 解码
        .generate_text(false)
        .generate_arbitrary(false)
        .reflect_mode(ReflectMode::Off) // 不用反射,也省掉内嵌 descriptor set
        .generate_with_setters(false) // 从不构造消息(测试 fixture 用 struct 字面量)
        .preserve_unknown_fields(false) // 从不重编码,顺带减小 view 解码开销
        // ---- 特性门控: 生成代码是本 crate 的内部实现,无条件生成 ----
        .gate_impls_on_crate_features(false)
        // ---- 命名与文件布局: 保持上游默认,生成物便于与 .proto 对照 ----
        .idiomatic_enum_aliases(true)
        .idiomatic_field_names(false)
        .file_per_package(false)
        .idiomatic_imports(false) // 实验性,且要求 file_per_package
        // ---- 解析语义 ----
        .strict_utf8_mapping(false) // 全树是 proto3,string 保持 String + UTF-8 校验
        .allow_message_set(false) // 树里没有 MessageSet,出现即报错
        // ---- owned 表示层: 固定为默认形态(热路径只用 view,不受这些影响)----
        .bytes_type(BytesRepr::Vec)
        .string_type(StringRepr::String)
        .map_type(MapRepr::HashMap)
        .box_type(PointerRepr::Inline)
        .repeated_type(RepeatedRepr::Vec)
        .compile()?;
    Ok(())
}
