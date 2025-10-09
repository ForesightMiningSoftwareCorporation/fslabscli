#[wasm_bindgen_test::wasm_bindgen_test]
async fn it_can_talk_to_custom_service_from_wasm() {
    let content = reqwest::get("http://127.0.0.1:44444/hello")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(content.trim(), "Hello!");
}
