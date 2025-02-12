use std::convert::TryFrom;
use vm::errors::{Error, CheckErrors, RuntimeErrorType, ShortReturnType};
use vm::types::{Value, SequenceData, TupleData, TypeSignature, QualifiedContractIdentifier, StandardPrincipalData, ListData, TupleTypeSignature};
use vm::contexts::{OwnedEnvironment};
use vm::database::MemoryBackingStore;
use vm::execute;

fn assert_executes(expected: Result<Value, Error>, input: &str) {
    assert_eq!(expected.unwrap(), execute(input).unwrap().unwrap());
}

#[test]
fn test_simple_tea_shop() {
    let test1 =
        "(define-map proper-tea ((tea-type int)) ((amount int)))
         (define-private (stock (tea int) (amount int))
           (map-set proper-tea (tuple (tea-type tea)) (tuple (amount amount))))
         (define-private (consume (tea int))
           (let ((current (unwrap!
                            (get amount (map-get? proper-tea (tuple (tea-type tea)))) 3)))
              (if (and (>= current 1))
                  (begin
                    (map-set proper-tea (tuple (tea-type tea))
                                           (tuple (amount (- current 1))))
                    1)
                  2)))
        (stock 1 3)
        (stock 2 5)
        (list (consume 1)
              (consume 1)
              (consume 2)
              (consume 2)
              (consume 2)
              (consume 1)
              (consume 1)
              (consume 2)
              (consume 2)
              (consume 2)
              (consume 2)
              (consume 3))
        ";

    let expected = Value::list_from(vec![
        Value::Int(1),
        Value::Int(1),
        Value::Int(1),
        Value::Int(1),
        Value::Int(1),
        Value::Int(1),
        Value::Int(2),
        Value::Int(1),
        Value::Int(1),
        Value::Int(2),
        Value::Int(2),
        Value::Int(3)],
    );

    assert_executes(expected, test1);
}

#[test]
fn test_bound_tuple() {
    let test =
        "(define-map kv-store ((key int)) ((value int)))
         (define-private (kv-add (key int) (value int))
            (begin
                (let ((my-tuple (tuple (key key))))
                (map-insert kv-store my-tuple (tuple (value value))))
            value))
         (define-private (kv-get (key int))
            (let ((my-tuple (tuple (key key))))
            (unwrap! (get value (map-get? kv-store my-tuple)) 0)))
         (define-private (kv-set (key int) (value int))
            (begin
                (let ((my-tuple (tuple (key key))))
                (map-set kv-store my-tuple
                                   (tuple (value value))))
                value))
         (define-private (kv-del (key int))
            (begin
                (let ((my-tuple (tuple (key key))))
                (map-delete kv-store my-tuple))
                key))
        ";

    let mut test_add_set_del = test.to_string();
    test_add_set_del.push_str("(list (kv-add 1 1) (kv-set 1 2) (kv-del 1) (kv-add 1 1))");
    let expected = Value::list_from(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(1),
        Value::Int(1)],
    );
    assert_executes(expected, &test_add_set_del);

    let mut test_get = test.to_string();
    test_get.push_str("(list (kv-get 1))");
    let expected = Value::list_from(vec![Value::Int(0)]);
    assert_executes(expected, &test_get);
}

#[test]
fn test_explicit_syntax_tuple() {
    let test =
        "(define-map kv-store ((key int)) ((value int)))
         (define-private (kv-add (key int) (value int))
            (begin
                (map-insert kv-store (tuple (key key))
                                    (tuple (value value)))
            value))
         (define-private (kv-get (key int))
            (unwrap! (get value (map-get? kv-store (tuple (key key)))) 0))
         (define-private (kv-set (key int) (value int))
            (begin
                (map-set kv-store (tuple (key key))
                                   (tuple (value value)))
                value))
         (define-private (kv-del (key int))
            (begin
                (map-delete kv-store (tuple (key key)))
                key))
        ";

    let mut test_add_set_del = test.to_string();
    test_add_set_del.push_str("(list (kv-add 1 1) (kv-set 1 2) (kv-del 1) (kv-add 1 1))");
    let expected = Value::list_from(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(1),
        Value::Int(1)],
    );
    assert_executes(expected, &test_add_set_del);

    let mut test_get = test.to_string();
    test_get.push_str("(list (kv-get 1))");
    let expected = Value::list_from(vec![Value::Int(0)]);
    assert_executes(expected, &test_get);
}

#[test]
fn test_implicit_syntax_tuple() {
    let test =
        "(define-map kv-store ((key int)) ((value int)))
         (define-private (kv-add (key int) (value int))
            (begin
                (map-insert kv-store {key: key}
                                     {value: value})
                value))
         (define-private (kv-get (key int))
            (unwrap! (get value (map-get? kv-store {key: key})) 0))
         (define-private (kv-set (key int) (value int))
            (begin
                (map-set kv-store {key : key}
                                  {value: value,})
                value))
         (define-private (kv-del (key int))
            (begin
                (map-delete kv-store {key: key})
                key))
        ";

    let mut test_add_set_del = test.to_string();
    test_add_set_del.push_str("(list (kv-add 1 1) (kv-set 1 2) (kv-del 1) (kv-add 1 1))");
    let expected = Value::list_from(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(1),
        Value::Int(1)],
    );
    assert_executes(expected, &test_add_set_del);

    let mut test_get = test.to_string();
    test_get.push_str("(list (kv-get 1))");
    let expected = Value::list_from(vec![Value::Int(0)]);
    assert_executes(expected, &test_get);
}

#[test]
fn test_set_int_variable() {
        let contract_src = r#"
        (define-data-var cursor int 0)
        (define-private (get-cursor)
            (var-get cursor))
        (define-private (set-cursor (value int))
            (if (var-set cursor value)
                value
                0))
        (define-private (increment-cursor)
            (begin
                (var-set cursor (+ 1 (get-cursor)))
                (get-cursor)))
    "#;

    let mut contract_src = contract_src.to_string();
    contract_src.push_str("(list (get-cursor) (set-cursor 8) (get-cursor) (set-cursor 255) (get-cursor) (increment-cursor))");
    let expected = Value::list_from(vec![
        Value::Int(0),
        Value::Int(8),
        Value::Int(8),
        Value::Int(255),
        Value::Int(255),
        Value::Int(256),
    ]);
    assert_executes(expected, &contract_src);
}

#[test]
fn test_set_bool_variable() {
    let contract_src = r#"
        (define-data-var is-okay bool true)
        (define-private (get-okay)
            (var-get is-okay))
        (define-private (set-okay (new-okay bool))
            (if (var-set is-okay new-okay)
                new-okay
                (get-okay)))
    "#;

    let mut contract_src = contract_src.to_string();
    contract_src.push_str("(list (get-okay) (set-okay false) (get-okay))");
    let expected = Value::list_from(vec![
        Value::Bool(true),
        Value::Bool(false),
        Value::Bool(false)
    ]);
    assert_executes(expected, &contract_src);
}

#[test]
fn test_set_tuple_variable() {
    let contract_src = r#"
        (define-data-var keys (tuple (k1 int) (v1 int)) (tuple (k1 1) (v1 1)))
        (define-private (get-keys)
            (var-get keys))
        (define-private (set-keys (value (tuple (k1 int) (v1 int))))
            (if (var-set keys value)
                value
                (get-keys)))
    "#;
    let mut contract_src = contract_src.to_string();
    contract_src.push_str("(list (get-keys) (set-keys (tuple (k1 2) (v1 0))) (get-keys))");
    let expected = Value::list_from(vec![
        Value::Tuple(TupleData::from_data(vec![("k1".into(), Value::Int(1)), ("v1".into(), Value::Int(1))]).unwrap()),
        Value::Tuple(TupleData::from_data(vec![("k1".into(), Value::Int(2)), ("v1".into(), Value::Int(0))]).unwrap()),
        Value::Tuple(TupleData::from_data(vec![("k1".into(), Value::Int(2)), ("v1".into(), Value::Int(0))]).unwrap()),
    ]);
    assert_executes(expected, &contract_src);
}

#[test]
fn test_set_response_variable() {
    let contract_src = r#"
        (define-data-var keys (response int bool) (ok 1))
        (var-set keys (err true))
        (var-set keys (ok 3))
        (unwrap! (var-get keys) 5)
    "#;
    let contract_src = contract_src.to_string();
    let expected = Value::Int(3);
    assert_executes(Ok(expected), &contract_src);

    let contract_src = r#"
        (define-data-var keys (response int bool) (ok 1))
        (var-set keys (err true))
        (unwrap! (var-get keys) 5)
    "#;
    let contract_src = contract_src.to_string();
    assert_eq!(Err(ShortReturnType::ExpectedValue(Value::Int(5)).into()),
               execute(&contract_src));

}

#[test]
fn test_set_list_variable() {
    let contract_src = r#"
        (define-data-var ranking (list 3 int) (list 1 2 3))
        (define-private (get-ranking)
            (var-get ranking))
        (define-private (set-ranking (new-ranking (list 3 int)))
            (if (var-set ranking new-ranking)
                new-ranking
                (get-ranking)))
    "#;

    let mut contract_src = contract_src.to_string();
    contract_src.push_str("(list (get-ranking) (set-ranking (list 2 3 1)) (get-ranking))");
    let expected = Value::list_from(vec![
        Value::list_from(vec![Value::Int(1), Value::Int(2), Value::Int(3)]).unwrap(),
        Value::list_from(vec![Value::Int(2), Value::Int(3), Value::Int(1)]).unwrap(),
        Value::list_from(vec![Value::Int(2), Value::Int(3), Value::Int(1)]).unwrap()
    ]);
    assert_executes(expected, &contract_src);
}

#[test]
fn test_get_list_max_len() {
    use vm::types::TypeSignature;
    let contract_src = r#"
        (define-data-var ranking (list 10 int) (list 1 2 3))
        (define-private (get-ranking)
            (var-get ranking))
    "#;

    let mut contract_src = contract_src.to_string();
    contract_src.push_str("(get-ranking)");

    let actual_value = execute(&contract_src).unwrap().unwrap();

    match actual_value {
        Value::Sequence(SequenceData::List(ListData { data, type_signature })) => {
            assert_eq!(vec![Value::Int(1), Value::Int(2), Value::Int(3)],
                       data);
            assert_eq!("(list 10 int)", &format!("{}", TypeSignature::from(type_signature)));
        },
        _ => panic!("Expected List")
    };
}

#[test]
fn test_set_string_variable() {
    let contract_src = r#"
        (define-data-var name (string-ascii 5) "alice")
        (define-private (get-name)
            (var-get name))
        (define-private (set-name (new-name (string-ascii 5)))
            (if (var-set name new-name)
                new-name
                (get-name)))
    "#;

    let mut contract_src = contract_src.to_string();
    contract_src.push_str("(list (get-name) (set-name \"celia\") (get-name))");
    let expected = Value::list_from(vec![
        Value::string_ascii_from_bytes("alice".to_string().into_bytes()).unwrap(),
        Value::string_ascii_from_bytes("celia".to_string().into_bytes()).unwrap(),
        Value::string_ascii_from_bytes("celia".to_string().into_bytes()).unwrap(),
    ]);
    assert_executes(expected, &contract_src);
}

#[test]
fn test_factorial_contract() {
    let test1 =
        "(define-map factorials ((id int)) ((current int) (index int)))
         (define-private (init-factorial (id int) (factorial int))
           (map-insert factorials {id: id} {current: 1, index: factorial}))
         (define-private (compute (id int))
           (let ((entry (unwrap! (map-get? factorials {id : id}) 0)))
                    (let ((current (get current entry))
                          (index   (get index entry)))
                         (if (<= index 1)
                             current
                             (begin
                               (map-set factorials {id: id}
                                                   {current: (* current index),
                                                    index: (- index 1)})
                               0)))))
        (init-factorial 1337 3)
        (init-factorial 8008 5)
        (list (compute 1337)
              (compute 1337)
              (compute 1337)
              (compute 1337)
              (compute 1337)
              (compute 8008)
              (compute 8008)
              (compute 8008)
              (compute 8008)
              (compute 8008)
              (compute 8008))
        ";

    let expected = Value::list_from(vec![
        Value::Int(0),
        Value::Int(0),
        Value::Int(6),
        Value::Int(6),
        Value::Int(6),
        Value::Int(0),
        Value::Int(0),
        Value::Int(0),
        Value::Int(0),
        Value::Int(120),
        Value::Int(120),
    ]);

    assert_executes(expected, test1);
}

#[test]
fn silly_naming_system() {
    let test1 =
        "(define-map silly-names ((name int)) ((owner int)))
         (define-private (register (name int) (owner int))
           (if (map-insert silly-names (tuple (name name)) (tuple (owner owner)))
               1 0))
         (define-private (who-owns? (name int))
           (let ((owner (get owner (map-get? silly-names (tuple (name name))))))
             (default-to (- 1) owner)))
         (define-private (invalidate! (name int) (owner int))
           (let ((current-owner (who-owns? name)))
                (if (is-eq current-owner owner)
                    (if (map-delete silly-names (tuple (name name))) 1 0)
                    0)))
        (list (register 0 0)
              (register 0 1)
              (register 1 1)
              (register 1 0)
              (who-owns? 0)
              (who-owns? 1)
              (invalidate! 0 1)
              (invalidate! 1 1)
              (who-owns? 0)
              (who-owns? 1))
        ";

    let expected = Value::list_from(vec![
        Value::Int(1),
        Value::Int(0),
        Value::Int(1),
        Value::Int(0),
        Value::Int(0),
        Value::Int(1),
        Value::Int(0),
        Value::Int(1),
        Value::Int(0),
        Value::Int(-1),
    ]);

    assert_executes(expected, test1);
}

#[test]
fn datamap_errors() {
    let tests = [
        "(map-get? non-existent (tuple (name 1)))",
        "(map-delete non-existent (tuple (name 1)))",
    ];

    for program in tests.iter() {
        assert_eq!(execute(program).unwrap_err(),
                   CheckErrors::NoSuchMap("non-existent".to_string()).into());
    }
}

#[test]
fn lists_system_2() {
    let test =
        "(define-map lists ((name int)) ((contents (list 5 1 int))))
         (define-private (add-list (name int) (content (list 5 1 int)))
           (map-insert lists (tuple (name name))
                                (tuple (contents content))))
         (define-private (get-list (name int))
            (get contents (map-get? lists (tuple (name name)))))
         (add-list 0 (list 1 2 3 4 5))
         (add-list 1 (list 1 2 3))
         (list      (get-list 0)
                    (get-list 1))
        (map-insert lists (tuple (name 1)) (tuple (contentious (list 1 2 6))))";

    match execute(test) {
        Err(Error::Unchecked(CheckErrors::TypeError(_,_))) => true,
        _ => false
    };
}

#[test]
fn lists_system() {
    let test1 =
        "(define-map lists ((name int)) ((contents (list 5 int))))
         (define-private (add-list (name int) (content (list 5 int)))
           (map-insert lists (tuple (name name))
                                (tuple (contents content))))
         (define-private (get-list (name int))
            (default-to (list) (get contents (map-get? lists (tuple (name name))))))
         (print (add-list 0 (list 1 2 3 4 5)))
         (print (add-list 1 (list 1 2 3)))
         (list      (get-list 0)
                    (get-list 1))
        ";

    let mut test_list_too_big = test1.to_string();
    test_list_too_big.push_str("(add-list 2 (list 1 2 3 4 5 6))");

    let mut test_bad_tuple_1 = test1.to_string();
    test_bad_tuple_1.push_str("(print (map-insert lists (tuple (name 1)) (print (tuple (contentious (list 1 2 6))))))");

    let mut test_bad_tuple_2 = test1.to_string();
    test_bad_tuple_2.push_str("(map-insert lists (tuple (name 1)) (tuple (contents (list 1 2 6)) (discontents 1)))");

    let mut test_bad_tuple_3 = test1.to_string();
    test_bad_tuple_3.push_str("(map-insert lists {name: 1} {contents: (list false true false)})");

    let mut test_bad_tuple_4 = test1.to_string();
    test_bad_tuple_4.push_str("(map-insert lists (tuple (name (list 1))) (tuple (contents (list 1 2 3))))");

    let expected = || {
        let list1 = Value::list_from(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
            Value::Int(5)])?;
        let list2 = Value::list_from(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3)])?;
        Value::list_from(vec![list1, list2])
    };

    assert_executes(expected(), test1);

    for test in [test_list_too_big, test_bad_tuple_1, test_bad_tuple_2,
                 test_bad_tuple_3, test_bad_tuple_4].iter() {
        let test = execute(test);
        println!("{:#?}", test);
        let expected_type_error = match test {
            Err(Error::Unchecked(CheckErrors::TypeValueError(_,_))) => true,
            _ => false
        };

        assert!(expected_type_error);
    }

}

#[test]
fn tuples_system() {
    let test1 =
        "(define-map tuples ((name int))
                            ((contents (tuple (name (string-ascii 5))
                                              (owner (string-ascii 5))))))

         (define-private (add-tuple (name int) (content (string-ascii 5)))
           (map-insert tuples (tuple (name name))
                                 (tuple (contents
                                   (tuple (name content)
                                          (owner content))))))
         (define-private (get-tuple (name int))
            (default-to \"\" (get name (get contents (map-get? tuples (tuple (name name)))))))


         (add-tuple 0 \"abcde\")
         (add-tuple 1 \"abcd\")
         (list      (get-tuple 0)
                    (get-tuple 1))
        ";

    let mut test_list_too_big = test1.to_string();
    test_list_too_big.push_str("(add-tuple 2 \"abcdef\")");

    let mut test_bad_tuple_1 = test1.to_string();
    test_bad_tuple_1.push_str("(map-insert tuples (tuple (name 1)) (tuple (contents (tuple (name \"abcde\") (owner \"abcdef\")))))");

    let mut test_bad_tuple_2 = test1.to_string();
    test_bad_tuple_2.push_str("(map-get? tuples (tuple (names 1)))");

    let mut test_bad_tuple_3 = test1.to_string();
    test_bad_tuple_3.push_str("(map-set tuples (tuple (names 1)) (tuple (contents (tuple (name \"abcde\") (owner \"abcde\")))))");

    let mut test_bad_tuple_4 = test1.to_string();
    test_bad_tuple_4.push_str("(map-set tuples (tuple (name 1)) (tuple (contents 1)))");

    let mut test_bad_tuple_5 = test1.to_string();
    test_bad_tuple_5.push_str("(map-delete tuples (tuple (names 1)))");

    let expected = || {
        let buff1 = Value::string_ascii_from_bytes("abcde".to_string().into_bytes())?;
        let buff2 = Value::string_ascii_from_bytes("abcd".to_string().into_bytes())?;
        Value::list_from(vec![buff1, buff2])
    };

    assert_executes(expected(), test1);

    let type_error_tests = [test_list_too_big, test_bad_tuple_1, test_bad_tuple_2, test_bad_tuple_3,
                            test_bad_tuple_4, test_bad_tuple_5];

    for test in type_error_tests.iter() {
        let expected_type_error = match execute(test) {
            Err(Error::Unchecked(CheckErrors::TypeValueError(_,_))) => true,
            _ => {
                println!("{:?}", execute(test));
                false
            }
        };

        assert!(expected_type_error);
    }

}

#[test]
fn bad_define_maps() {
    let tests = [
        "(define-map lists ((name int)) ((contents int bool)))",
        "(define-map lists ((name int)) (contents bool))",
        "(define-map lists ((name int)) contents)",
        "(define-map (lists) ((name int)) contents)",
        "(define-map lists ((name int)) contents 5)",
        "(define-map lists ((name int)) ((contents (list 5 0 int))))",
    ];
    let mut expected: Vec<Error> = vec![
        CheckErrors::BadSyntaxExpectedListOfPairs.into(),
        CheckErrors::BadSyntaxExpectedListOfPairs.into(),
        CheckErrors::BadSyntaxExpectedListOfPairs.into(),
        CheckErrors::ExpectedName.into(),
        CheckErrors::IncorrectArgumentCount(3, 4).into(),
        CheckErrors::InvalidTypeDescription.into()
    ];

    for (test, expected_err) in tests.iter().zip(expected.drain(..)) {
        let outcome = execute(test).unwrap_err();
        assert_eq!(outcome, expected_err);
    }

}

#[test]
fn bad_tuples() {
    let tests = ["(tuple (name 1) (name 3))",
                 "(tuple name 1)",
                 "(tuple (name 1) (blame))",
                 "(get value (tuple (name 1)))",
                 "(get name five (tuple (name 1)))",
                 "(get 1234 (tuple (name 1)))"];
    let mut expected = vec![
        CheckErrors::NameAlreadyUsed("name".into()),
        CheckErrors::BadSyntaxBinding,
        CheckErrors::BadSyntaxBinding,
        CheckErrors::NoSuchTupleField(
            "value".into(),
            TupleTypeSignature::try_from(vec![("name".into(), TypeSignature::IntType)]).unwrap()),
        CheckErrors::IncorrectArgumentCount(2, 3),
        CheckErrors::ExpectedName
    ];

    for (test, expected_err) in tests.iter().zip(expected.drain(..)) {
        let outcome = execute(test).unwrap_err();
        assert_eq!(outcome, expected_err.into());
    }
}
