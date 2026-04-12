#[derive(Debug, toasty::Model)]
struct User {
    #[key]
    #[auto]
    id: u64,

    name: String,

    #[unique]
    email: String,
}

pub async fn run() -> toasty::Result<()> {
    let mut db = toasty::Db::builder()
        .models(toasty::models!(User))
        .connect("sqlite:./test.db")
        .await?;

    db.push_schema().await?;

    let user = toasty::create!(User {
        name: "Alice",
        email: "alice@example.com",
    })
    .exec(&mut db)
    .await?;

    println!("Created: {:?}", user.name);

    let found = User::get_by_id(&mut db, &user.id).await?;
    println!("Found: {:?}", found.email);

    Ok(())
}
